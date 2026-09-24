//! `hytte-plugin-clock-demo` — the reference out-of-process widget plugin for
//! trollshell's "frontend B" plugin architecture (issue #35; on the #266 wire
//! protocol, the #272 host transport, and the #275 `hytte-plugin` runtime) —
//! and, since #1388, the **two-instance demo** as well.
//!
//! # First: the reference plugin
//!
//! It is the **end-to-end proof** that a plugin can live outside the shell,
//! link **no GTK** (only [`hytte_plugin`] — not even tokio directly), and
//! drive a real widget over a Unix socket. Left where its manifest puts it, it
//! renders a clock card into the shell's [`Mount::SidebarTop`] slot and, when
//! that card's button is clicked, asks the host to open the power menu —
//! exercising the render path, the state-subscription path, and the
//! event→effect round-trip in one demo. That is the shape a new plugin author
//! reads first, and it is unchanged.
//!
//! # Second: one binary, two surfaces
//!
//! The same binary also renders a **bar chip**: a compact `HH:MM` seven-segment
//! readout plus, on a click, its own drawer page
//! (`Effect::OpenPage(Page::PluginSelf)`). Which of the two a process draws is
//! decided by the **family of its effective mount** — [`Mount::is_bar`] over
//! [`hytte_plugin::effective_mount`] (#1317), i.e. by the `HYTTE_PLUGIN_MOUNT`
//! the launch set (#1159) and nothing else. There is no flag, no env knob of
//! this crate's own, and no second config file.
//!
//! Running both at once is therefore a **deployment** shape, not a packaging
//! one: two `programs.trollshell.plugins.<id>` entries pointing at this one
//! package, the second with `mount = "BarCenter"`. The attribute name is the
//! launch id, and the module renders `HYTTE_PLUGIN_ID` for it automatically
//! (#1250/#1284), because the host allows exactly one live connection per
//! plugin id.
//!
//! Until #1388 those two surfaces were two crates — `hytte-plugin-clock-demo`
//! and `hytte-plugin-bar-clock-demo` — which is the same plugin twice over
//! (Annika on #1163: "since the plugins can be launched multiple times it makes
//! no sense to have a bar-clock-demo and a clock-demo"). `hytte-plugin-stats`
//! (#1250/#1251) and `hytte-claude-bridge` (#1315) are the same split; this is
//! the smallest possible statement of it.
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
//! The manifest is **per binary**, not per instance, so it declares the union
//! of what the two surfaces need: one mount (the sidebar default a launch may
//! override), one subscription (`Clock`), and one capability
//! ([`Capability::OpenPage`], which both arms use — for the power menu on the
//! card and for this plugin's own page on the chip).
//!
//! `update`, `view`, the `HH:MM` projection **and the split itself** are
//! unit-tested below — that is the demo's main correctness signal, since the
//! live host isn't reachable here; the session loop itself is covered by
//! `hytte-plugin`'s own tests.
//!
//! # The bar chip is also the bar-side showcase for the preem display seam
//!
//! The chip's `HH:MM` is a [`hytte_plugin::display::SevenSeg`] readout rather
//! than a `Node::Label`, which makes this the bar-mounted half of the #884
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
//! The drawer page stays plain GTK labels, and so does the sidebar card: an
//! RFC3339 timestamp and a raw unix count are text, not a retro readout, and
//! the contrast is the point — the seam is opt-in per widget, not a mode the
//! plugin enters.
//!
//! ## What the raster arm costs, stated (#898 review R5)
//!
//! Against a shell that does not speak preem, a **bar** instance's `view()`
//! rasterises a 188×70 frame — 52,640 bytes — on **every** `Clock` snapshot,
//! i.e. about once a second, while the `HH:MM` it draws changes once a minute.
//! The runtime's render dedup then throws 59 of every 60 away, after the
//! allocation, the rasterise and a 52 KB compare. That is not new (`timer` has
//! always cost exactly this for its bar readout) and it is not a bug, but it is
//! the honest price of a pixel chip on a 1 Hz cadence, and it is the *only*
//! mode live until the shell renderer lands. It is left as it stands
//! deliberately: a cache keyed on [`clock_face`] would need interior mutability
//! in `view(&self)` and would make the reference plugin less readable than the
//! thing it references. In state mode the same chip is a ~40-byte node. A
//! sidebar instance pays none of it — its card is labels.

use hytte_plugin::display::{SevenSeg, StyleName};
use hytte_plugin::proto::{
    Capability, Dir, Effect, EventKind, Manifest, Mount, Node, Page, StateKey,
};
use hytte_plugin::{CmdSender, Input, Plugin, View};

/// Stable plugin id — the host's mount-slot ownership key and audit-log
/// subject, and what a **second** instance of this binary must be given a
/// different one of (`HYTTE_PLUGIN_ID`, #1250), since the host allows one live
/// connection per id.
const PLUGIN_ID: &str = "clock-demo";

/// Where this plugin mounts when the launch says nothing.
///
/// [`Mount::SidebarTop`] because this is first of all the **reference** plugin,
/// and the card is the surface a new author should meet: a plain `Box` of a
/// `Label` and a `Button`, with no display seam and no second tree. A bar
/// instance is the deliberate opt-in of a `mount = "BarCenter"` on its own
/// `programs.trollshell.plugins` entry.
const DEFAULT_MOUNT: Mount = Mount::SidebarTop;

/// Node ids for the **sidebar card**. `CLOCK_BTN` is the click event target (a
/// `Button` requires an id).
const ROOT_ID: &str = "clock-demo-root";
const TIME_ID: &str = "clock-demo-time";
const CLOCK_BTN: &str = "clock-demo-btn";

/// Node ids for the **bar chip** and the drawer page it opens — disjoint from
/// the card's, so one `update` can tell the two surfaces' click targets apart
/// without a second dispatch table.
///
/// [`CHIP_TIME_ID`] keys the host reconciler onto the *same* preem renderer
/// instance across renders (#882's `preem_id` rule), which is what lets the
/// shell own the widget's continuity in state mode and swap the texture in
/// place in raster mode.
const CHIP_ID: &str = "clock-demo-chip";
const CHIP_TIME_ID: &str = "clock-demo-chip-time";
/// The clickable chip button — its `Click` opens the plugin's own page.
const CHIP_BTN: &str = "clock-demo-chip-btn";
/// Page ids: the page root, the full ISO timestamp and the unix seconds.
const PAGE_ID: &str = "clock-demo-page";
const PAGE_ISO_ID: &str = "clock-demo-page-iso";
const PAGE_UNIX_ID: &str = "clock-demo-page-unix";

/// The all-dash face the chip's readout wears before the first snapshot lands,
/// and whenever the host's timestamp doesn't project to an `HH:MM` — see
/// [`clock_face`].
const NO_CLOCK: &str = "--:--";

/// The plugin's entire state. Lives here — the host never stores or
/// round-trips it; it is rebuilt on every (re)connect and re-derived from the
/// next snapshot.
#[derive(Debug, PartialEq, Eq)]
struct ClockDemo {
    /// Latest ISO-8601 local timestamp from the host's clock subscription.
    iso: String,
    /// Latest unix seconds (kept to show the full projected `ClockState`).
    unix: i64,
    /// Which surface this instance is, resolved once at [`Plugin::init`] from
    /// the launch's effective mount. Not a knob and not derived from anything
    /// in the model: a process is a chip or a card for its whole life.
    is_bar: bool,
    /// The chip's seven-segment readout (#884). Config only — a seven-segment
    /// strip is pure, so this carries no animation state and needs no
    /// `advance`; the text is handed to `node` at render time.
    ///
    /// Held by both arms rather than only the bar one: it is a style name in a
    /// struct, the model stays one shape, and a sidebar instance simply never
    /// renders it.
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
/// The narrowing came with #884 and it is the one thing the widget swap
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

impl ClockDemo {
    /// The seed model for one launch, with the mount override read through an
    /// injected `lookup` rather than the process environment.
    ///
    /// This is the whole of [`Plugin::init`] — `init` does nothing but call it
    /// with the real `std::env::var` — which is deliberate, on
    /// `hytte-claude-bridge`'s `resolve_settings` precedent (#1315 review, MED
    /// 3): the pure half ([`hytte_plugin::effective_mount_from`]) is already
    /// covered by the SDK's own tests, so what a test here has to reach is the
    /// line that *threads* the answer into the model. With the composition
    /// living here, hardcoding `is_bar` goes red in the `tests` module's
    /// `the_launch_mount_picks_the_surface`.
    ///
    /// **That is not the whole of it**, and saying so was this function's own
    /// review finding (#1389): [`Plugin::init`]'s one line — the `lookup` it
    /// passes — is a second place the wiring lives, and no test built on this
    /// seam can reach it, because every one of them supplies its own `lookup`.
    /// Neutering `init` to `with_launch(&|_| None)` therefore left all 13
    /// tests green while shipping a bar instance that renders the sidebar
    /// card. `init_reads_the_real_process_environment` is what closes it: a
    /// re-exec'd child of the test binary with a real `HYTTE_PLUGIN_MOUNT` in
    /// its environment, calling `init` itself. The tree has closed this same
    /// hole three times (`hytte-plugin`'s `runtime`, `hytte-claude-bridge`'s
    /// `plugin`, `hytte-plugin-stats`' `plugin`), each as a review finding.
    ///
    /// `&dyn Fn` rather than a generic: `unsafe_code = "forbid"` rules out
    /// `std::env::set_var` (an `unsafe fn` in edition 2024), so a test cannot
    /// drive the real environment at all and this seam is the only way in.
    fn with_launch(lookup: &dyn Fn(&str) -> Option<String>) -> Self {
        Self {
            // Placeholder time until the first snapshot lands (the runtime
            // renders this seed immediately, so the slot mounts right away).
            iso: "—".to_owned(),
            unix: 0,
            is_bar: hytte_plugin::effective_mount_from(DEFAULT_MOUNT, lookup).is_bar(),
            // VFD: the same skin the timer's bar readout wears, so the two
            // seven-segment chips in the bar match.
            seg: SevenSeg::new(StyleName::Vfd),
        }
    }

    /// The effect a click on `node` produces, which depends on **which surface
    /// this instance renders**.
    ///
    /// Gated on [`is_bar`](Self) rather than on the node id alone. The
    /// two id sets are disjoint, so matching ids would give the same answer for
    /// every event the host can actually send — but then a click routed for the
    /// surface this process does *not* draw would still fire an effect, and the
    /// split would be a property of the id constants rather than of the
    /// instance. This way `view` and `update` state the same thing.
    fn on_click(&self, node: &str) -> Vec<Effect> {
        match (self.is_bar, node) {
            // The chip opens the plugin's own drawer page (#349 PR2). The host
            // resolves `PluginSelf` to *this* plugin's page by the effect's
            // plugin id — no page name to know.
            (true, CHIP_BTN) => vec![Effect::OpenPage(Page::PluginSelf)],
            // The card opens a page of the shell's, which is the other half of
            // what the capability buys and the reason the reference plugin
            // asks for it.
            (false, CLOCK_BTN) => vec![Effect::OpenPage(Page::PowerMenu)],
            _ => Vec::new(),
        }
    }

    /// The **sidebar card**: a vertical `Box` holding the formatted time
    /// (`ts-clock`, the host's monospace/tabular clock class) above a `Button`
    /// that opens the power menu. Publishes no page — a card that is not a
    /// click target has nothing to open, and a page nothing can reach is a
    /// surface the drawer would mount and the user could never see.
    fn card(&self) -> View {
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
                    tooltip: None,
                },
                Node::Button {
                    id: CLOCK_BTN.to_owned(),
                    classes: Vec::new(),
                    child: Box::new(Node::Label {
                        id: None,
                        text: "Power menu".to_owned(),
                        classes: Vec::new(),
                        tooltip: None,
                    }),
                },
            ],
            tooltip: None,
        }
        .into()
    }

    /// The **bar chip** and the drawer page its click opens (#349).
    ///
    /// The chip — wrapped by the host in a `.ts-plugin-chip` pill — is a
    /// horizontal `Box` holding a [`Node::Button`] (the click target) whose
    /// child is the compact `HH:MM` seven-segment readout. The page is a
    /// vertical `Box` showing the full projected `ClockState` — the RFC3339
    /// timestamp and the raw unix seconds — a second, independent tree distinct
    /// from the compact chip. Its root carries **no** `.card`/`.ts-plugin-*`
    /// class: the drawer supplies the card chrome, so the page owns only its
    /// inner content.
    ///
    /// The one `node` call is the whole #884 seam: it lands as a typed
    /// `Node::Preem` or a rasterised `Node::Pixels` depending on what the host
    /// advertised, with no branch here. The readout carries no CSS class — the
    /// `ts-clock` the card's label wears is a monospace/tabular *font* rule, and
    /// this chip is a pixel surface with its own font baked in.
    fn chip(&self) -> View {
        let chip = Node::Box {
            id: Some(CHIP_ID.to_owned()),
            dir: Dir::Horizontal,
            spacing: 4,
            scroll: false,
            classes: Vec::new(),
            children: vec![Node::Button {
                id: CHIP_BTN.to_owned(),
                classes: Vec::new(),
                child: Box::new(self.seg.node(CHIP_TIME_ID, &clock_face(&self.iso))),
            }],
            tooltip: None,
        };
        View::new(chip).panel(Node::Box {
            id: Some(PAGE_ID.to_owned()),
            dir: Dir::Vertical,
            spacing: 6,
            scroll: false,
            classes: Vec::new(),
            children: vec![
                Node::Label {
                    id: Some(PAGE_ISO_ID.to_owned()),
                    text: self.iso.clone(),
                    classes: vec!["title-2".to_owned()],
                    tooltip: None,
                },
                Node::Label {
                    id: Some(PAGE_UNIX_ID.to_owned()),
                    text: format!("unix: {}", self.unix),
                    classes: vec!["dim-label".to_owned()],
                    tooltip: None,
                },
            ],
            tooltip: None,
        })
    }
}

impl Plugin for ClockDemo {
    /// Purely host-driven: no timers, no fetches, no self-generated messages.
    type Msg = std::convert::Infallible;

    /// Purely display: it issues no I/O of its own, so it has no commands and
    /// ignores the command lane entirely (see `hytte_plugin`'s *Commands*
    /// docs). `Infallible` = "no command can ever be constructed".
    type Cmd = std::convert::Infallible;

    /// Subscribes to `Clock`, mounts [`DEFAULT_MOUNT`], requests the
    /// [`Capability::OpenPage`] capability. `Manifest::new` stamps
    /// `proto = PROTO_VERSION`, which the host exact-matches at the handshake.
    ///
    /// One manifest for both surfaces: it is per binary, not per instance. The
    /// capability list is therefore the **union** of what the two arms use, and
    /// that union happens to be one entry — the card needs `OpenPage` for the
    /// power menu and the chip needs it for `Page::PluginSelf`. Still nothing
    /// else: no `RunCommand`, no `Notify`, no `OpenUri`. The tests assert the
    /// list is exactly `[OpenPage]` rather than that it lacks something,
    /// because the host auto-grants every manifest capability.
    fn manifest() -> Manifest {
        let mut m = Manifest::new(PLUGIN_ID, DEFAULT_MOUNT).with_version(env!("CARGO_PKG_VERSION"));
        m.subscribes = vec![StateKey::Clock];
        m.capabilities = vec![Capability::OpenPage];
        m
    }

    /// The seed model, with this launch's mount resolved once — see
    /// [`ClockDemo::with_launch`], which is the whole of this function. The
    /// command sender goes unused: this plugin only reads state and asks the
    /// host to open a page.
    fn init(_cmds: CmdSender<Self::Cmd>) -> Self {
        Self::with_launch(&|key| std::env::var(key).ok())
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
            // Our only interactive node is this surface's button; what its
            // click opens is `on_click`'s business. The effect rides exactly
            // one render frame, so a clock tick never re-fires it.
            Input::Event {
                node,
                kind: EventKind::Click,
                ..
            } => self.on_click(&node),
            // Any other interaction, effect result, or sidebar-visibility push
            // (#288) is a no-op that never touches the view — no `RunCommand`
            // is issued (so no `EffectResult` is expected), and neither surface
            // has pollers to park. Listed rather than wildcarded so a new
            // host→plugin frame is a compile error here, which is the place to
            // decide whether this demo cares.
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

    /// Project the model into the declarative widget tree the host reconciles
    /// into GTK — **which** tree being the one decision this plugin makes about
    /// its own placement (#1388).
    ///
    /// A bar instance renders the chip and publishes the page a click opens; a
    /// sidebar instance renders the card and publishes no page.
    /// `hytte-plugin-stats`' `Stats::view` and `hytte-claude-bridge`'s are the
    /// same split, for the same reason.
    fn view(&self) -> View {
        if self.is_bar {
            self.chip()
        } else {
            self.card()
        }
    }
}

fn main() {
    hytte_plugin::run::<ClockDemo>()
}

#[cfg(test)]
mod tests {
    use super::{
        CHIP_BTN, CHIP_ID, CHIP_TIME_ID, CLOCK_BTN, ClockDemo, DEFAULT_MOUNT, NO_CLOCK, PLUGIN_ID,
        ROOT_ID, clock_face, short_time,
    };
    use hytte_plugin::display::{RenderMode, StyleName, testing::with_render_mode};
    use hytte_plugin::preem::{DisplayStyle, seven_seg};
    use hytte_plugin::proto::preem::PreemWidget;
    use hytte_plugin::proto::{
        Capability, ClockState, Dir, Effect, EventKind, Mount, Node, Page, PluginMsg, StateKey,
        StateSnapshot, decode, encode,
    };
    use hytte_plugin::{Input, Plugin};

    /// The variable spelled as a **literal**, never through the SDK's own
    /// private `MOUNT_ENV` const: these tests are a statement about the
    /// documented launch contract (`docs/plugin-env.md`), and deriving the name
    /// from the thing under test would follow a rename straight past the
    /// deployment that did not get one.
    const MOUNT_ENV: &str = "HYTTE_PLUGIN_MOUNT";

    fn clock_snapshot(iso: &str, unix: i64) -> Input<std::convert::Infallible> {
        Input::Snapshot(StateSnapshot {
            clock: Some(ClockState {
                iso: iso.to_owned(),
                unix,
            }),
        })
    }

    /// A model as a launch that set [`MOUNT_ENV`] to `mount` would build it —
    /// the one seam these tests have on the split, since `std::env::set_var` is
    /// `unsafe` and the crate forbids unsafe.
    fn launched_at(mount: Mount) -> ClockDemo {
        ClockDemo::with_launch(&move |key| (key == MOUNT_ENV).then(|| mount.wire_name().to_owned()))
    }

    /// A fresh **sidebar** model: a launch that says nothing, so the manifest's
    /// own [`DEFAULT_MOUNT`] stands. The demo issues no commands, so no command
    /// lane is built at all.
    fn fresh() -> ClockDemo {
        ClockDemo::with_launch(&|_| None)
    }

    /// A fresh **bar** model — the same binary, launched into the bar.
    fn fresh_bar() -> ClockDemo {
        launched_at(Mount::BarCenter)
    }

    /// The id of a tree's root `Box`, which is what tells the two surfaces
    /// apart without touching either one's contents (or, in raster mode, its
    /// pixel buffer).
    fn root_id(tree: &Node) -> Option<&str> {
        match tree {
            Node::Box { id, .. } => id.as_deref(),
            _ => None,
        }
    }

    // ── The split (#1388) ───────────────────────────────────────────────────

    /// **The pin on the fold**: one binary, two surfaces, chosen by the
    /// family of the launch's effective mount and by nothing else.
    ///
    /// Swept over all nine mounts rather than one of each, with `Mount::is_bar`
    /// — the wire's own family line — as the oracle, so a mount added to the
    /// vocabulary is covered the day it lands. The chain under test is the
    /// whole one: `HYTTE_PLUGIN_MOUNT` → [`hytte_plugin::effective_mount_from`]
    /// → the model's `is_bar` → which tree `view` returns.
    ///
    /// Falsified before shipping: swapping `view`'s two arms turns this red on
    /// the first mount it checks.
    #[test]
    fn the_launch_mount_picks_the_surface() {
        for mount in Mount::ALL {
            let model = launched_at(mount);
            // State mode: nothing in either tree is a pixel buffer here, so a
            // failure prints ids rather than 52 KB of RGBA.
            let view = with_render_mode(RenderMode::State, || model.view());
            if mount.is_bar() {
                assert_eq!(root_id(&view.tree), Some(CHIP_ID), "{mount:?} is a bar");
                assert!(
                    view.panel.is_some(),
                    "a bar chip publishes the page its click opens ({mount:?})",
                );
            } else {
                assert_eq!(root_id(&view.tree), Some(ROOT_ID), "{mount:?} is a sidebar");
                assert!(
                    view.panel.is_none(),
                    "a card is not a click target, so it has no page to publish ({mount:?})",
                );
            }
        }

        // …and a launch that says nothing at all is the card: this is first of
        // all the reference plugin, and `DEFAULT_MOUNT` is what its manifest
        // ships with.
        assert!(!DEFAULT_MOUNT.is_bar());
        let seed = with_render_mode(RenderMode::State, || fresh().view());
        assert_eq!(root_id(&seed.tree), Some(ROOT_ID));
        assert!(seed.panel.is_none());
    }

    /// Set (to any value) only on the re-exec'd child that actually runs
    /// [`init_reads_the_real_process_environment_inner`] — the same marker
    /// shape as `hytte-plugin::runtime`'s `MOUNT_ENV_CHILD` and
    /// `hytte-claude-bridge`'s `INIT_ENV_CHILD`, and for the same reason: an
    /// ordinary `cargo test` run discovers the inner test like any other and
    /// must not try to run its scenario with no launch environment set.
    const INIT_ENV_CHILD: &str = "HYTTE_PLUGIN_CLOCK_DEMO_INIT_TEST_CHILD";

    /// Printed by the child only once its scenario has run to completion and
    /// passed, so the parent can tell "the scenario passed" from "the
    /// `--exact` filter matched no test and libtest still reports `0 passed`,
    /// exit 0" — the failure mode a renamed inner test produces.
    const INIT_ENV_CHILD_OK: &str = "init-env-child-reached-the-end";

    /// **[`Plugin::init`] itself reads the real process environment** — the
    /// one production line every other test in this module routes around
    /// (#1389 review, HIGH 1).
    ///
    /// The whole suite reaches the model through [`ClockDemo::with_launch`]
    /// with a `lookup` of its own, so `init`'s `&|key| std::env::var(key).ok()`
    /// is never executed by it: neutering that line to `&|_| None` left 13
    /// tests, clippy and the whole of `nix flake check` green while shipping a
    /// **bar instance that renders the sidebar card** — a unit that starts
    /// cleanly, stays running and says nothing. The tree has closed this exact
    /// seam-wrapper hole three times before, each as a review finding
    /// (`hytte-plugin`'s `the_mount_env_var_reaches_the_register_frame`,
    /// `hytte-claude-bridge`'s `init_reaches_the_view_with_a_real_process_environment`,
    /// `hytte-plugin-stats`' `settings_reads_the_real_process_environment`);
    /// this is the fourth and it is the one #1388 asked for by name, since the
    /// ask was "exactly the way `hytte-plugin-stats` does it".
    ///
    /// A **child process** rather than a `set_var`: `unsafe_code = "forbid"`
    /// makes `std::env::set_var` (an `unsafe fn` in edition 2024) unspellable
    /// here, so the only way to hand this process's own `getenv` a value is to
    /// be a different process — `Command::env` is the safe builder that does
    /// it.
    ///
    /// **Two children, because they catch different mutations.** The override
    /// child sets `HYTTE_PLUGIN_MOUNT=BarCenter` and wants the chip:
    /// `BarCenter` rather than a sidebar name precisely because
    /// [`DEFAULT_MOUNT`] is a sidebar mount, so a fully neutered `init`
    /// (falling back to the manifest) and a working one give *different*
    /// answers — the inner test asserts that premise before anything else.
    /// The neutral child removes the variable and wants the card, which is
    /// what catches the opposite mutation (an `init` that hands over a
    /// constant `Some("BarCenter")`, which the override child would happily
    /// pass) and is also the one place this crate proves its shipped default
    /// survives `init` unmolested, in the same real-process harness.
    #[test]
    fn init_reads_the_real_process_environment() {
        let inner = "tests::init_reads_the_real_process_environment_inner";
        let args = ["--exact", "--nocapture", "--test-threads=1", inner];
        assert!(
            args.contains(&"--exact"),
            "the re-exec must stay filtered to exactly one inner test",
        );
        let exe = std::env::current_exe().expect("this test binary's own path");

        let overridden = std::process::Command::new(&exe)
            .args(args)
            .env(INIT_ENV_CHILD, "1")
            .env(MOUNT_ENV, "BarCenter")
            .output()
            .expect("re-exec this test binary with the mount override set");
        assert_init_child_reached_the_end(&overridden, inner, "override");

        // `env_remove`, not merely "unset": a child inherits the parent's
        // environment, so a developer running `cargo test` from a shell that
        // happens to export `HYTTE_PLUGIN_MOUNT` would otherwise get a neutral
        // child that is not neutral.
        let neutral = std::process::Command::new(&exe)
            .args(args)
            .env(INIT_ENV_CHILD, "1")
            .env_remove(MOUNT_ENV)
            .output()
            .expect("re-exec this test binary with no mount override");
        assert_init_child_reached_the_end(&neutral, inner, "neutral");
    }

    /// A child scenario both exited 0 **and** reached its own end marker.
    ///
    /// The second half is not redundant: `--exact` against a renamed inner
    /// test matches nothing, libtest reports `0 passed` and exits 0, and the
    /// whole pin goes inert — the failure mode both precedents call out.
    fn assert_init_child_reached_the_end(out: &std::process::Output, inner: &str, which: &str) {
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "the {which} child scenario failed ({:?})\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
            out.status,
        );
        assert!(
            stdout.contains(INIT_ENV_CHILD_OK),
            "the {which} child exited 0 without reaching the end of {inner} — a stale \
             filter matches no test and libtest still reports success\n\
             --- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        );
    }

    /// The scenario body of [`init_reads_the_real_process_environment`]. Does
    /// nothing at all unless the parent's marker is set, so an ordinary
    /// `cargo test` run — which discovers it like any other test — does not
    /// try to run it with no launch environment set up for it.
    #[test]
    fn init_reads_the_real_process_environment_inner() {
        if std::env::var_os(INIT_ENV_CHILD).is_none() {
            return;
        }
        // The premise the parent's choice of override rests on, asserted
        // rather than assumed (`hytte-plugin-stats`' copy of this, #1327
        // review LOW 1): if `DEFAULT_MOUNT` ever moved to a bar region — not a
        // hypothetical on a plugin whose whole point is running on both
        // families — a neutered `init` and a working one would agree on the
        // override child and this test would quietly stop catching anything.
        assert!(
            !DEFAULT_MOUNT.is_bar(),
            "test setup: the override's family must differ from DEFAULT_MOUNT's own, \
             or a neutered init() and a working one give the same answer",
        );

        let (tx, _rx) = hytte_plugin::cmd_channel();
        // `init`, not `with_launch`: reaching the seam's wrapper is the entire
        // point of being a separate process.
        let model = ClockDemo::init(tx);
        let view = with_render_mode(RenderMode::State, || model.view());

        match std::env::var(MOUNT_ENV).ok().as_deref() {
            Some("BarCenter") => {
                assert_eq!(
                    root_id(&view.tree),
                    Some(CHIP_ID),
                    "init must resolve the surface from the REAL environment",
                );
                assert!(view.panel.is_some(), "a bar instance publishes its page");
            }
            None => {
                assert_eq!(
                    root_id(&view.tree),
                    Some(ROOT_ID),
                    "with nothing set, init must land on the manifest's own mount",
                );
                assert!(view.panel.is_none(), "a card has no page to publish");
            }
            other => panic!("unexpected child environment: {MOUNT_ENV}={other:?}"),
        }
        println!("{INIT_ENV_CHILD_OK}");
    }

    /// The other half of the split: an instance answers only the click target
    /// it actually renders, so `update` and `view` state the same thing.
    #[test]
    fn each_surface_answers_only_its_own_button() {
        let mut bar = fresh_bar();
        assert_eq!(
            bar.update(Input::event(CHIP_BTN, EventKind::Click)),
            vec![Effect::OpenPage(Page::PluginSelf)],
            "the chip opens this plugin's own page",
        );
        assert!(
            bar.update(Input::event(CLOCK_BTN, EventKind::Click))
                .is_empty(),
            "a bar instance never renders the card's button",
        );

        let mut card = fresh();
        assert_eq!(
            card.update(Input::event(CLOCK_BTN, EventKind::Click)),
            vec![Effect::OpenPage(Page::PowerMenu)],
            "the card opens the shell's power menu",
        );
        assert!(
            card.update(Input::event(CHIP_BTN, EventKind::Click))
                .is_empty(),
            "a sidebar instance never renders the chip",
        );
    }

    /// One manifest for both surfaces: the sidebar default, the `Clock`
    /// subscription, and the **union** of the two arms' capabilities — which is
    /// the one entry both of them need.
    #[test]
    fn the_manifest_is_the_union_of_both_surfaces() {
        let m = ClockDemo::manifest();
        assert_eq!(m.id, PLUGIN_ID);
        assert_eq!(m.mount, DEFAULT_MOUNT, "the reference surface is the card");
        assert_eq!(m.subscribes, vec![StateKey::Clock]);
        assert_eq!(
            m.capabilities,
            vec![Capability::OpenPage],
            "exactly this, not merely `no RunCommand`: the host auto-grants \
             every capability a manifest names",
        );
    }

    // ── The sidebar card ────────────────────────────────────────────────────

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
                    tooltip: None,
                },
                Node::Button {
                    id: "clock-demo-btn".to_owned(),
                    classes: vec![],
                    child: Box::new(Node::Label {
                        id: None,
                        text: "Power menu".to_owned(),
                        classes: vec![],
                        tooltip: None,
                    }),
                },
            ],
            tooltip: None,
        };
        assert_eq!(model.view().tree, expected);
    }

    /// A click on a node we don't own is ignored (no spurious effect).
    #[test]
    fn click_on_unknown_node_is_ignored() {
        let mut model = fresh();
        let effects = model.update(Input::event("not-ours", EventKind::Click));
        assert!(effects.is_empty());
    }

    // ── The bar chip ────────────────────────────────────────────────────────

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
        // The seed — one codepoint with no seven-segment glyph, so without the
        // fallback the chip is a single dark cell.
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
    /// model, and a bar instance's `view` renders the exact compact chip the
    /// host will reconcile — a rasterised seven-segment readout
    /// **byte-identical** to the `preem::seven_seg(…).into_node(…)` a plugin
    /// author writes by hand.
    ///
    /// That equality is the migration's compat promise: this chip must reach an
    /// un-advertising shell as the same pixels it would have had if #884 had
    /// never happened, and only comparing the buffers proves it.
    #[test]
    fn against_an_old_shell_the_chip_is_a_rasterised_seven_seg() {
        let mut model = fresh_bar();
        let effects = model.update(clock_snapshot("2026-07-11T15:49:00+02:00", 1_752_241_740));
        assert!(effects.is_empty());
        assert_eq!(model.iso, "2026-07-11T15:49:00+02:00");
        assert_eq!(model.unix, 1_752_241_740);

        let expected = Node::Box {
            id: Some("clock-demo-chip".to_owned()),
            dir: Dir::Horizontal,
            spacing: 4,
            scroll: false,
            classes: vec![],
            children: vec![Node::Button {
                id: "clock-demo-chip-btn".to_owned(),
                classes: vec![],
                child: Box::new(
                    seven_seg("15:49", DisplayStyle::Vfd).into_node(Some(CHIP_TIME_ID), vec![]),
                ),
            }],
            tooltip: None,
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
        let mut model = fresh_bar();
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
                assert_eq!(id.as_deref(), Some(CHIP_TIME_ID), "the reconciler's key");
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

    /// #349 PR2: a click on the chip button opens the plugin's own page, and
    /// that page projects the full `ClockState` (a tree distinct from the
    /// chip).
    #[test]
    fn the_chips_page_renders_the_full_clock() {
        let mut model = fresh_bar();
        model.update(clock_snapshot("2026-07-11T15:49:00+02:00", 1_752_241_740));

        let expected_panel = Node::Box {
            id: Some("clock-demo-page".to_owned()),
            dir: Dir::Vertical,
            spacing: 6,
            scroll: false,
            classes: vec![],
            children: vec![
                Node::Label {
                    id: Some("clock-demo-page-iso".to_owned()),
                    text: "2026-07-11T15:49:00+02:00".to_owned(),
                    classes: vec!["title-2".to_owned()],
                    tooltip: None,
                },
                Node::Label {
                    id: Some("clock-demo-page-unix".to_owned()),
                    text: "unix: 1752241740".to_owned(),
                    classes: vec!["dim-label".to_owned()],
                    tooltip: None,
                },
            ],
            tooltip: None,
        };
        // The page is plain GTK either way — the seam is per widget, not a mode
        // the plugin enters — so pin it in *both* render modes.
        for mode in [RenderMode::Raster, RenderMode::State] {
            let panel = with_render_mode(mode, || model.view().panel);
            assert_eq!(panel, Some(expected_panel.clone()), "{mode:?}");
        }
    }

    // ── Both surfaces ───────────────────────────────────────────────────────

    /// A snapshot whose `clock` is `None` (startup window) changes nothing on
    /// either surface — the runtime's tree dedup then sends no frame for it.
    #[test]
    fn snapshot_without_clock_changes_nothing() {
        for mut model in [fresh(), fresh_bar()] {
            let arm = if model.is_bar { "bar" } else { "sidebar" };
            for mode in [RenderMode::Raster, RenderMode::State] {
                let before = with_render_mode(mode, || model.view());
                let effects = model.update(Input::Snapshot(StateSnapshot::default()));
                assert!(effects.is_empty());
                // `==` rather than `assert_eq!`: in raster mode the chip's view
                // carries a `Node::Pixels`, whose `Debug` would dump the buffer.
                assert!(
                    with_render_mode(mode, || model.view()) == before,
                    "{arm} / {mode:?}",
                );
            }
        }
    }

    /// The `Register` frame built from this plugin's manifest is valid on the
    /// wire, and declares the #882 vocabulary negotiation — which is what makes
    /// the host send the `Hello` that unlocks the state arm above.
    ///
    /// One frame for both surfaces, because there is one manifest: an instance
    /// registers the same way whichever tree it goes on to render.
    #[test]
    fn the_register_frame_round_trips_and_negotiates() {
        let reg = PluginMsg::Register {
            manifest: ClockDemo::manifest(),
        };
        let back: PluginMsg = decode(&encode(&reg)).expect("register frame decodes");
        assert_eq!(reg, back);

        let PluginMsg::Register { manifest } = &reg else {
            panic!("built as a Register frame")
        };
        assert!(manifest.negotiates_vocab());
    }

    /// The `Render` frame each surface produces is valid on the wire, in both
    /// render modes (#884): the typed state node has to survive the codec
    /// exactly as the rasterised buffer already did, since that frame is the
    /// only thing the shell ever sees.
    #[test]
    fn both_surfaces_render_frames_round_trip_in_both_modes() {
        for mut model in [fresh(), fresh_bar()] {
            let arm = if model.is_bar { "bar" } else { "sidebar" };
            let _ = model.update(clock_snapshot("2026-07-11T15:49:00+02:00", 1));
            for mode in [RenderMode::Raster, RenderMode::State] {
                // The bar arm's is panel-bearing (the chip button + its page,
                // #349); the sidebar arm's is the card alone.
                let view = with_render_mode(mode, || model.view());
                let render = PluginMsg::Render {
                    tree: view.tree,
                    panel: view.panel.map(Box::new),
                    hidden_on: view.hidden_on,
                    effects: vec![Effect::OpenPage(Page::PluginSelf)],
                };
                let back: PluginMsg = decode(&encode(&render)).expect("render frame decodes");
                assert!(render == back, "{arm} / {mode:?}");
            }
        }
    }
}
