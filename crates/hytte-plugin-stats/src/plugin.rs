//! The TEA core: manifest, model, `update`, `view`.
//!
//! Everything the host can do to this plugin arrives as an [`Input`] and
//! everything it does about it is a fold into the model plus a re-projection —
//! so the whole plugin is testable by handing [`Stats::update`] a snapshot
//! literal, which is what the tests here do. No `/proc` is read in any of them;
//! the two things that touch the filesystem are `sample::Sampler` and the Top
//! apps `top_apps::Walker` (crate-private modules, so these are plain names
//! rather than links), which live behind the command lane in
//! [`Stats::sources`].

use std::collections::VecDeque;
use std::sync::OnceLock;

use hytte_plugin::proto::{Capability, Effect, EventKind, Manifest, Mount, Page, StateKey};
use hytte_plugin::tokio_stream::wrappers::UnboundedReceiverStream;
use hytte_plugin::{CmdReceiver, CmdSender, Input, MsgStream, Plugin, View};

use crate::card::{self, Widgets};
use crate::config::{self, Family};
use crate::panel;
use crate::sample;
use crate::sample::{Cmd, Msg, Snapshot};
use crate::top_apps::TopApps;

/// The manifest id this binary ships with — the **first** instance's identity.
///
/// A second instance of the same binary overrides it at launch with
/// `HYTTE_PLUGIN_ID` (#1250); see `docs/plugin-env.md`. It is a `const` rather
/// than a string literal at the one call site so a test can assert the shipped
/// id without restating it.
pub const PLUGIN_ID: &str = "stats";

/// Where the card mounts when the launch says nothing.
///
/// `SidebarRightTop` because that is the thing Annika asked for on Discussion
/// #1235 ("prime candidate for right side sidebar") and what P1 exists to put
/// on glass. A deployment moves it with `HYTTE_PLUGIN_MOUNT` like any other
/// plugin, and moving it into a *bar* region additionally switches which table
/// of `stats.toml` it reads — that is [`config::Family`]'s whole job.
pub const DEFAULT_MOUNT: Mount = Mount::SidebarRightTop;

/// The launch-resolved settings: which table this instance reads, and what that
/// table said.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    /// Which half of `stats.toml` this instance is running on.
    pub family: Family,
    /// That half, resolved.
    pub card: config::Card,
}

/// Resolve the settings for one launch. Pure — the mount override is read
/// through `lookup` (via [`hytte_plugin::effective_mount_from`], graduated
/// into the SDK by #1317 out of what used to be this crate's own `mount`
/// module) and the config is handed in, so every branch is testable without an
/// environment or a file.
#[must_use]
pub fn settings_from(
    manifest_mount: Mount,
    lookup: &dyn Fn(&str) -> Option<String>,
    stats: &config::Stats,
) -> Settings {
    let family = Family::of(hytte_plugin::effective_mount_from(manifest_mount, lookup));
    Settings {
        family,
        card: stats.for_family(family),
    }
}

/// The settings for **this** process, resolved once.
///
/// Once per process rather than once per session: both [`Plugin::init`] and
/// [`Plugin::sources`] need them, `sources` is an associated function with no
/// `&self` to reach a value `init` computed, and reading the file twice per
/// reconnect would let the two halves of one session disagree about the poll
/// cadence if the file changed in between.
///
/// The consequence, stated because it is a real one: **an edit to `stats.toml`
/// takes effect when the unit restarts**, not live. `hytte-config`'s `watch`
/// feature is what would change that, and it is deliberately off here — it
/// pulls `futures-signals` into a plugin that has no reactive handle to publish
/// into, and a card whose cadence can change underneath its own poll gate is a
/// second mechanism (`Gate::set_period`) this phase does not need.
fn settings() -> Settings {
    static SETTINGS: OnceLock<Settings> = OnceLock::new();
    *SETTINGS.get_or_init(|| {
        let stats = config::load();
        let resolved = settings_from(DEFAULT_MOUNT, &|key| std::env::var(key).ok(), &stats);
        tracing::info!(
            table = resolved.family.table(),
            poll_secs = resolved.card.poll.as_secs(),
            "stats: reading the [{}] table of stats.toml",
            resolved.family.table(),
        );
        resolved
    })
}

/// The plugin's whole state.
#[derive(Debug)]
pub struct Stats {
    /// Which surface this instance is: the bar chips plus the drawer page, or
    /// the sidebar card. Decided by the launch mount (`HYTTE_PLUGIN_MOUNT`),
    /// not by anything in the model.
    family: Family,
    /// This instance's resolved `stats.toml` table.
    cfg: config::Card,
    /// The latest sample. Defaults to the "nothing measured yet" state, which
    /// the seed render draws as dashes — every reading of it, headline
    /// included (#1277 LOW 5).
    snapshot: Snapshot,
    /// The overall-load history the sidebar card's (and the CPU chip's) scope
    /// sweeps, newest last, capped at the scope's own column count.
    ring: VecDeque<f32>,
    /// The drawer page's history lines (#1252) — plain samples in each
    /// line's own unit, sixty apiece, for the page's `Node::Sparkline`s.
    ///
    /// Kept apart from [`ring`](Self) on purpose: that one is the sidebar
    /// card's, in the scope's `-1.0..=1.0` range over 144 columns, and the
    /// card is not changing. Replaces #1295's disk-I/O and VRAM scope rings
    /// and the session-peak denominator the disk ring needed — an auto-scaled
    /// sparkline over the window *is* the native row's windowed max.
    page: panel::History,
    /// The drawer page's latest Top apps walk (#1419 item 3). Empty while the
    /// page is shut — the walker is parked then, and a reading nobody is
    /// taking any more is not drawn as if it were live.
    top_apps: TopApps,
    /// Whether this instance's own page is on screen — the host's
    /// `PageVisibility` push (#1427), and the `/proc` walker's gate. `false`
    /// until the host's register seed says otherwise.
    page_open: bool,
    /// Which of the drawer page's expanders are open — the one piece of UI
    /// state this plugin holds, because the wire's `Node::Expander` is
    /// plugin-driven: a click on an expander's id flips its flag and the next
    /// render carries it. All collapsed by default, like the native rows.
    /// Pure presentation: no flag gates any sampling.
    expanded: panel::Expanded,
    /// The preem widgets, held across renders so the shell keeps one renderer
    /// instance per node (and so the raster fallback keeps its animation).
    widgets: Widgets,
    /// The command lane to the sampling tasks: the host's surface visibility,
    /// forwarded so a closed sidebar parks the poller, and the page's, so a
    /// shut page parks the `/proc` walker.
    cmds: CmdSender<Cmd>,
}

impl Stats {
    /// Fold one fresh sample into the model.
    ///
    /// Split out of [`Plugin::update`] so the tests can drive it directly with
    /// a `Snapshot` literal — which is also the only way they can, since the
    /// real sampler is on the other side of a `spawn_blocking`.
    pub fn apply(&mut self, snapshot: Snapshot) {
        // The loads, never a core *count*: the pitch is fitted to the widest
        // row the wrap produces, which `card::row_cells` is the one definition
        // of (#1277 MEDIUM 1).
        self.widgets.fit_cores(&snapshot.per_core);
        self.widgets
            .set_gpu(snapshot.gpu.as_ref().and_then(|g| g.load), self.dt());
        self.widgets.set_memory(snapshot.memory.as_ref());

        // The drawer page's lines (#1252): one point per reading this tick
        // actually has, none for one it withholds — see `panel::History`.
        self.page.push(&snapshot);

        // A withheld reading is not a sample: a cold tick (or one whose
        // `/proc/stat` read failed) must not push a fake rest value onto the
        // trace, and must not restate the scope's batch either.
        if let Some(cpu) = snapshot.cpu {
            self.ring.push_back(card::trace_sample(cpu));
            while self.ring.len() > card::HISTORY_COLS as usize {
                self.ring.pop_front();
            }
            // `make_contiguous` is why the ring is a `VecDeque` and not a `Vec`
            // with a rotating index: the scope wants one slice, and this is the
            // cheap way to hand it one without copying on every render.
            let ring: Vec<f32> = self.ring.iter().copied().collect();
            self.widgets.push_history(&ring);
        }

        self.snapshot = snapshot;
    }

    /// The heartbeat interval, as the raster-path animations measure it.
    fn dt(&self) -> f32 {
        self.cfg.poll.as_secs_f32()
    }

    /// A model with an explicit family and config — the seam every test in this
    /// module uses, so none of them touches the process environment or the real
    /// XDG search path.
    ///
    /// # A bar instance opens its own gate
    ///
    /// [`hytte_plugin::poll::Gate`] starts **closed**, and the only thing that
    /// opens it is a hidden→visible edge on the command lane. A sidebar card
    /// gets that edge from the host's visibility task; a **bar** mount gets one
    /// constant `SlotVisibility { visible: true }` at register
    /// (`trollshell/src/plugins/session.rs`: "a bar chip is effectively always
    /// on-screen … seed a constant `visible: true` for bar mounts and hold no
    /// task"), which does open the gate — but that is the *host's* answer to a
    /// question a bar chip should not have to ask, and it is delivered with a
    /// `try_send` on a bounded channel.
    ///
    /// So a bar instance opens the gate **itself**, here, by putting its own
    /// `SetVisible(true)` on the lane before anything else can. The lane is
    /// unbounded and `init` runs before `sources` in the same session
    /// (`hytte_plugin::runtime`), so the sampler's first read off the gate is
    /// this command, the first sample lands within a scheduler turn rather than
    /// within a poll period, and **a bar chip samples on its own tick whatever
    /// the host says about visibility** — which is the only correct answer for
    /// a widget that is always on screen. The host's own seed then arrives as a
    /// redundant level (`opens(true, true)` is false) and does nothing.
    ///
    /// The other half of the same decision is in [`Plugin::update`]: a bar
    /// instance does not forward `SlotVisible` at all, so nothing can park it.
    #[must_use]
    pub fn with_config(family: Family, cfg: config::Card, cmds: CmdSender<Cmd>) -> Self {
        if family == Family::Bar {
            // A dropped receiver means the session is already tearing down,
            // which is fine to ignore — the same tolerance `update` has.
            let _ = cmds.send(Cmd::SetVisible(true));
        }
        Self {
            family,
            cfg,
            snapshot: Snapshot::default(),
            ring: VecDeque::new(),
            page: panel::History::default(),
            top_apps: TopApps::default(),
            page_open: false,
            expanded: panel::Expanded::default(),
            widgets: Widgets::default(),
            cmds,
        }
    }
}

impl Plugin for Stats {
    type Msg = Msg;
    type Cmd = Cmd;

    /// Subscribes to [`StateKey::SlotVisible`] and [`StateKey::PageVisible`],
    /// and asks for exactly one capability, [`Capability::OpenPage`].
    ///
    /// `SlotVisible` is not optional for this plugin: it is the only thing
    /// that lets the sampler park while the sidebar is closed, and
    /// `docs/plugin-env.md` makes it the requirement for any plugin that can be
    /// moved across mount families — which this one is built to be. A bar
    /// instance ignores it entirely and opens its own gate; see
    /// [`Stats::with_config`].
    ///
    /// `PageVisible` (#1427) is what parks the drawer page's Top apps `/proc`
    /// walk while the page is shut, as native parks its own; see
    /// [`Plugin::page_visible`] below. Without it the host never calls that
    /// hook and the walker never runs at all. Its one cost: the key rides in
    /// the `Register` frame, and a shell built before #1427 cannot decode it,
    /// so this plugin will not register with one — which a bundled plugin,
    /// shipped with its shell, never meets. A manifest is per binary, so both
    /// instances subscribe.
    ///
    /// `OpenPage` is what a click on a bar chip needs
    /// ([`Effect::OpenPage`]`(`[`Page::PluginSelf`]`)`, #1251) — the host
    /// resolves `PluginSelf` to this plugin's own `panel` tree by the effect's
    /// plugin id, and drops the effect with a warning if the capability is not
    /// on the manifest. A manifest is **per binary**, not per instance, so a
    /// sidebar instance declares it and never uses it: its card is not a click
    /// target and it publishes no panel.
    ///
    /// Still **not** `RunCommand` (nothing here launches anything), not
    /// `Notify`, not `OpenUri`. The list is asserted as exactly `[OpenPage]`
    /// rather than as "does not contain `RunCommand`", because the host
    /// auto-grants every manifest capability and the only safe assertion is the
    /// whole list.
    fn manifest() -> Manifest {
        let mut m = Manifest::new(PLUGIN_ID, DEFAULT_MOUNT).with_version(env!("CARGO_PKG_VERSION"));
        m.subscribes = vec![StateKey::SlotVisible, StateKey::PageVisible];
        m.capabilities = vec![Capability::OpenPage];
        m
    }

    fn init(cmds: CmdSender<Self::Cmd>) -> Self {
        let settings = settings();
        Self::with_config(settings.family, settings.card, cmds)
    }

    /// The sampling tasks: one set per session — the sensors sampler behind
    /// the surface's visibility gate, and the Top apps `/proc` walker behind
    /// the page's — plus the router that splits this session's one command
    /// lane between them. Their messages come back as `Msg::Sampled` and
    /// `Msg::TopApps` (the message type is crate-private, so these are plain
    /// names rather than links).
    fn sources(cmds: CmdReceiver<Self::Cmd>) -> Option<MsgStream<Self::Msg>> {
        let (msg_tx, msg_rx) = hytte_plugin::cmd_channel::<Msg>();
        let settings = settings();
        // The handles are dropped: every task ends by itself when this
        // session's lane closes.
        let _ = sample::spawn(
            cmds,
            msg_tx,
            settings.card.poll,
            sample::Needs::of(settings.card),
        );
        Some(Box::pin(UnboundedReceiverStream::new(msg_rx)))
    }

    fn update(&mut self, input: Input<Self::Msg>) -> Vec<Effect> {
        match input {
            // A fresh sample: the only thing that moves this card.
            Input::App(Msg::Sampled(snapshot)) => self.apply(*snapshot),
            // A fresh Top apps walk (#1419 item 3) — kept only while the page
            // is open. The gate never cancels a walk already in flight, so one
            // can land just after the page closed; drawing it would put a
            // reading back on the lists `page_visible` has just cleared.
            Input::App(Msg::TopApps(apps)) => {
                if self.page_open {
                    self.top_apps = apps;
                }
            }
            // The visibility gate (#288): forward down the command lane so the
            // sampler parks and resumes. A dropped receiver means the session is
            // tearing down, which is fine to ignore.
            //
            // A **bar** instance does not forward it at all. The host seeds a
            // constant `true` there and holds no task, so the only pushes a bar
            // chip could ever see are that seed (redundant — the gate is
            // already open, see `with_config`) and a hypothetical `false`,
            // which would park a chip that is on screen. Dropping them makes
            // "a bar chip samples on its own tick" a property of this plugin
            // rather than a property of the host's current behaviour.
            Input::SlotVisible(visible) => {
                if self.family == Family::Sidebar {
                    let _ = self.cmds.send(Cmd::SetVisible(visible));
                }
            }
            // A click on any chip opens this plugin's own drawer page (#1251).
            // All four chips do the same thing, so the ids exist to be distinct
            // reconciler keys rather than to be told apart here — but the set is
            // still checked, so a click the host forwards for some other node
            // cannot open a page.
            Input::Event {
                node,
                kind: EventKind::Click,
                ..
            } if card::is_chip_button(&node) => return vec![Effect::OpenPage(Page::PluginSelf)],
            // The drawer page's Disk card (#1252): the wire's expander is
            // plugin-driven, so its header click lands here and the next
            // render carries the flipped flag. No effect — it is page-local.
            Input::Event {
                node,
                kind: EventKind::Click,
                ..
            } if node == panel::DISKS_EXPANDER_ID => self.expanded.disks = !self.expanded.disks,
            // The two Top apps lists (#1419 item 3): plugin-driven like the
            // Disk card, and like it pure presentation — the walker follows
            // the page, not the lists (#1427), so a collapsed header keeps its
            // `name · value` as native's does. No effect either.
            Input::Event {
                node,
                kind: EventKind::Click,
                ..
            } if node == panel::TOP_APPS_CPU_ID => self.expanded.top_cpu = !self.expanded.top_cpu,
            Input::Event {
                node,
                kind: EventKind::Click,
                ..
            } if node == panel::TOP_APPS_RAM_ID => self.expanded.top_ram = !self.expanded.top_ram,
            // Everything else is a push this plugin never subscribed to, or an
            // answer to an effect it never emits. Listed rather than wildcarded
            // so a new host→plugin frame is a compile error here — the place to
            // decide whether this card cares — rather than a silent no-op.
            Input::Snapshot(_)
            | Input::EffectResult { .. }
            | Input::AudioSpectrum(_)
            | Input::Event { .. }
            | Input::ConsentDecision { .. }
            | Input::CalendarUpcoming(_)
            | Input::SessionLocked(_)
            | Input::NowPlaying(_)
            | Input::DatasourceQuery { .. }
            | Input::DatasourceResult { .. } => {}
        }
        Vec::new()
    }

    /// This instance's own page opened or closed (#1427): the Top apps
    /// walker's gate, forwarded down the lane exactly as `hytte_plugin::poll`'s
    /// *Gating on your own page* shows.
    ///
    /// **Page visible alone**, not "page visible and a list expanded": native
    /// walks whenever its drawer page is open, and its collapsed headers show
    /// the heaviest app — see `crate::top_apps` for the argument.
    ///
    /// Every family forwards it. The bar instance's page is its chips' drawer
    /// page; the sidebar instance publishes none, so the host only ever seeds
    /// it `false` and its walker stays parked. Either way it is a separate
    /// lane from the surface's [`Cmd::SetVisible`]: a page close never parks
    /// the sensors sampler, so a bar chip keeps sampling on its own tick
    /// whatever the drawer does.
    ///
    /// Forwarded on every call, not only on a change: the host sends the seed
    /// and then edges, [`hytte_plugin::poll::Gate`] acts on edges of what it is
    /// sent, and the SDK's rule for this hook is latest-wins.
    ///
    /// A close drops the lists, so a reopen never shows a reading taken before
    /// it; the walk that lands next is measured after the reopen.
    fn page_visible(&mut self, visible: bool) -> Vec<Effect> {
        self.page_open = visible;
        if !visible {
            self.top_apps = TopApps::default();
        }
        // A dropped receiver means the session is tearing down, which is fine
        // to ignore — the same tolerance `update` has.
        let _ = self.cmds.send(Cmd::PageVisible(visible));
        Vec::new()
    }

    /// The bar instance renders the chips **and** publishes the drawer page a
    /// click opens; the sidebar instance renders P1's card and publishes no
    /// panel.
    ///
    /// The panel is family-gated rather than always attached, because a panel
    /// nothing can open is a surface the drawer will mount and the user can
    /// never reach — the host parks any published panel in its mailbox whether
    /// or not an effect ever names it. Making the sidebar card a click target
    /// too is one `Node::Button` away and deliberately not P2's call: it would
    /// change the card #1250 put on glass.
    fn view(&self) -> View {
        match self.family {
            Family::Bar => {
                View::new(card::chips(self.cfg, &self.snapshot, &self.widgets)).panel(panel::panel(
                    self.cfg,
                    &self.snapshot,
                    &self.page,
                    &self.top_apps,
                    self.expanded,
                ))
            }
            Family::Sidebar => card::card(self.cfg, &self.snapshot, &self.widgets).into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_MOUNT, PLUGIN_ID, Settings, Stats, settings_from};
    use crate::card::HISTORY_COLS;
    use crate::config::{self, Card, Family};
    use crate::sample::{Cmd, Gpu, Msg, Snapshot};
    use crate::top_apps::TopApps;
    use hytte_plugin::proto::{
        Capability, Effect, EventKind, Mount, Node, Page, PluginMsg, SPARKLINE_VOCAB, StateKey,
        decode, encode,
    };
    use hytte_plugin::{CmdReceiver, Input, Plugin};

    fn fresh(cfg: Card) -> Stats {
        Stats::with_config(Family::Sidebar, cfg, hytte_plugin::cmd_channel().0)
    }

    /// A bar instance and the command lane it seeds, so a test can read what it
    /// put there.
    fn fresh_bar(cfg: Card) -> (Stats, CmdReceiver<Cmd>) {
        let (tx, rx) = hytte_plugin::cmd_channel();
        (Stats::with_config(Family::Bar, cfg, tx), rx)
    }

    fn sample(cpu: f32) -> Input<Msg> {
        Input::App(Msg::Sampled(Box::new(Snapshot {
            cpu: Some(cpu),
            per_core: vec![cpu; 4],
            cpu_temp_c: Some(50.0),
            gpu: Some(Gpu {
                name: "test".to_owned(),
                load: Some(cpu),
                temperature_c: Some(44.0),
                ..Gpu::default()
            }),
            ..Snapshot::default()
        })))
    }

    /// The shipped manifest: the id a single instance registers under, the
    /// right sidebar, the two subscriptions that make the two gates work, and
    /// **exactly one capability**.
    ///
    /// The subscription list is asserted whole too. `PageVisible` is the one
    /// entry nothing else can catch going missing: the host sends a
    /// `PageVisibility` push only to a plugin that asked (#305), and every
    /// other test here calls `page_visible` directly, so without this the
    /// walker would silently never run and the suite would stay green.
    ///
    /// The capability list is the security-relevant half and is asserted as the
    /// *whole list* rather than as "does not contain RunCommand": the host
    /// auto-grants every manifest capability, so the only safe assertion is the
    /// whole list. P1 asserted it empty; P2 (#1251) adds `OpenPage` and nothing
    /// else, which is what a chip click needs and the ceiling on what this
    /// plugin can ask the shell to do.
    ///
    /// **Falsified** by adding any second capability.
    #[test]
    fn the_manifest_asks_for_the_right_sidebar_and_exactly_open_page() {
        let m = Stats::manifest();
        assert_eq!(m.id, PLUGIN_ID);
        assert_eq!(m.id, "stats");
        assert_eq!(m.mount, DEFAULT_MOUNT);
        assert_eq!(m.mount, Mount::SidebarRightTop);
        assert_eq!(
            m.capabilities,
            vec![Capability::OpenPage],
            "a chip click opens this plugin's own page, and that is the whole ask",
        );
        assert!(
            !m.capabilities.contains(&Capability::RunCommand),
            "nothing here launches anything",
        );
        assert_eq!(
            m.subscribes,
            vec![StateKey::SlotVisible, StateKey::PageVisible],
            "the surface's push parks the sampler behind a closed sidebar, and the \
             page's (#1427) is the only thing that ever runs the Top apps walker",
        );
    }

    /// **A click on any chip opens this plugin's own drawer page** — exactly
    /// one effect, and the same one from all four chips.
    ///
    /// **Falsified** by returning `Vec::new()` from the click arm (no page
    /// opens) or by dropping the `is_chip_button` guard (the last assertion
    /// below then reds, because any node's click opens a page).
    #[test]
    fn a_click_on_a_chip_opens_the_plugins_own_page() {
        let (mut model, _rx) = fresh_bar(Card::bar_default());
        let _ = model.update(sample(0.5));

        for class in crate::card::CHIP_CLASSES {
            let id = crate::card::chip_button_id(class);
            assert_eq!(
                model.update(Input::event(id.clone(), EventKind::Click)),
                vec![Effect::OpenPage(Page::PluginSelf)],
                "{id}",
            );
        }

        // …and nothing else is a page-open. A scroll on a chip is not a click,
        // and a click on a node this plugin does not own is not ours.
        assert!(
            model
                .update(Input::event(
                    crate::card::chip_button_id("cpu"),
                    EventKind::Scroll { dx: 0.0, dy: 1.0 },
                ))
                .is_empty(),
        );
        for other in ["stats-card", "stats-chip-services", "stats-chips", "", "x"] {
            assert!(
                model
                    .update(Input::event(other, EventKind::Click))
                    .is_empty(),
                "{other}",
            );
        }
    }

    /// **The bar instance publishes the drawer page; the sidebar instance does
    /// not** — a panel nothing can open is a surface the drawer mounts and
    /// nobody can reach.
    ///
    /// **Falsified** by attaching the panel unconditionally.
    #[test]
    fn only_the_bar_instance_publishes_a_panel() {
        let (mut bar, _rx) = fresh_bar(Card::bar_default());
        let _ = bar.update(sample(0.5));
        let view = bar.view();
        assert!(view.panel.is_some(), "the bar instance carries its page");
        assert_ne!(
            view.panel.as_ref(),
            Some(&view.tree),
            "…and the page is a different tree from the chips",
        );

        let mut side = fresh(Card::sidebar_default());
        let _ = side.update(sample(0.5));
        assert!(
            side.view().panel.is_none(),
            "P1's card is not a click target and publishes no page",
        );
    }

    /// The two families render two different trees off one snapshot — which is
    /// the whole "one binary, two instances" claim, made at the level the host
    /// actually sees.
    #[test]
    fn the_family_decides_which_surface_is_rendered() {
        let (mut bar, _rx) = fresh_bar(Card::bar_default());
        let mut side = fresh(Card::sidebar_default());
        let _ = bar.update(sample(0.5));
        let _ = side.update(sample(0.5));
        assert_ne!(bar.view().tree, side.view().tree);
    }

    /// **A bar instance opens its own poll gate**: `with_config` puts exactly
    /// one `SetVisible(true)` on the command lane, which is the hidden→visible
    /// edge `hytte_plugin::poll::Gate` needs (it starts closed) and the reason a
    /// bar chip does not depend on the host's constant seed frame.
    ///
    /// The other end of this — that one open edge then keeps the cadence
    /// running with a silent lane — is
    /// `crate::sample`'s `one_open_edge_then_silence_keeps_the_cadence_running`.
    ///
    /// **Falsified** by deleting the seed in `with_config`: the lane is empty,
    /// the gate never opens on its own, and the chip's reading depends entirely
    /// on a `try_send` in the host.
    #[test]
    fn a_bar_instance_seeds_its_own_open_edge_and_a_sidebar_one_does_not() {
        let (_bar, mut rx) = fresh_bar(Card::bar_default());
        assert_eq!(rx.try_recv(), Ok(Cmd::SetVisible(true)));
        assert!(rx.try_recv().is_err(), "exactly one, not a stream of them");

        let (tx, mut rx) = hytte_plugin::cmd_channel();
        let _side = Stats::with_config(Family::Sidebar, Card::sidebar_default(), tx);
        assert!(
            rx.try_recv().is_err(),
            "a sidebar card waits for the host's visibility task",
        );
    }

    /// **A bar instance ignores a visibility push entirely**, so nothing can
    /// park a chip that is on screen; a sidebar instance forwards it, which is
    /// what makes a closed sidebar free.
    ///
    /// **Falsified** by forwarding unconditionally: the `false` push then
    /// reaches the lane and a bar chip stops sampling.
    #[test]
    fn a_bar_instance_ignores_a_visibility_push() {
        let (mut bar, mut rx) = fresh_bar(Card::bar_default());
        assert_eq!(rx.try_recv(), Ok(Cmd::SetVisible(true)), "the seed");
        for push in [false, true, false] {
            assert!(bar.update(Input::SlotVisible(push)).is_empty());
        }
        assert!(
            rx.try_recv().is_err(),
            "no host push reaches a bar instance's sampler",
        );

        let (tx, mut rx) = hytte_plugin::cmd_channel();
        let mut side = Stats::with_config(Family::Sidebar, Card::sidebar_default(), tx);
        assert!(side.update(Input::SlotVisible(false)).is_empty());
        assert_eq!(rx.try_recv(), Ok(Cmd::SetVisible(false)));
    }

    /// **The mechanism**, end to end from a launch to a table: the mount
    /// override decides the family, and the family decides the card.
    ///
    /// **Falsified** by having `settings_from` return `stats.sidebar`
    /// unconditionally — the bar row then reports the sidebar card.
    #[test]
    fn the_launch_mount_decides_which_table_this_instance_reads() {
        /// A lookup that answers `value` for the placement variable and nothing
        /// for anything else, asserting which variable was asked for from a
        /// **literal** — so a renamed const cannot pass by agreeing with itself.
        fn env(value: Option<&str>) -> impl Fn(&str) -> Option<String> + use<'_> {
            move |key| {
                assert_eq!(key, "HYTTE_PLUGIN_MOUNT");
                value.map(str::to_owned)
            }
        }

        let stats = config::Stats::default();

        // No override: the manifest's own mount, which is a sidebar one.
        assert_eq!(
            settings_from(DEFAULT_MOUNT, &env(None), &stats),
            Settings {
                family: Family::Sidebar,
                card: stats.sidebar,
            },
        );
        // Moved to a bar region by the launch → the [bar] table, without the
        // plugin's own manifest changing at all.
        assert_eq!(
            settings_from(DEFAULT_MOUNT, &env(Some("BarRight")), &stats),
            Settings {
                family: Family::Bar,
                card: stats.bar,
            },
        );
        // …and moved to the *other* sidebar it is still the sidebar table.
        assert_eq!(
            settings_from(DEFAULT_MOUNT, &env(Some("SidebarBottom")), &stats).family,
            Family::Sidebar,
        );
    }

    /// Every one of the nine mounts resolves to the table its family names —
    /// driven off `Mount::ALL`, so a tenth mount cannot land without an answer.
    #[test]
    fn all_nine_mounts_resolve_to_a_table() {
        let stats = config::Stats::default();
        for mount in Mount::ALL {
            let lookup = |key: &str| {
                assert_eq!(key, "HYTTE_PLUGIN_MOUNT");
                Some(mount.wire_name().to_owned())
            };
            let settings = settings_from(DEFAULT_MOUNT, &lookup, &stats);
            assert_eq!(settings.family, Family::of(mount), "{}", mount.wire_name());
            // Against the table **by name**, never against
            // `stats.for_family(settings.family)` — that would assert the
            // selection against itself and stay green with the whole mechanism
            // deleted (measured: neutering `for_family` to `self.sidebar`
            // leaves this row passing, which is why the expectation is spelled
            // out here).
            assert_eq!(
                settings.card,
                if mount.is_bar() {
                    stats.bar
                } else {
                    stats.sidebar
                },
                "{} must read the [{}] table",
                mount.wire_name(),
                settings.family.table(),
            );
        }
    }

    /// A sample changes the view; an identical one does not.
    ///
    /// The second half is what the runtime's render dedup rides on, and a `NaN`
    /// anywhere in the tree would defeat it forever (#896/#898) — so this is
    /// also the test that would catch an unsanitised reading reaching `view`.
    #[test]
    fn a_sample_moves_the_view_and_an_identical_one_does_not() {
        let mut model = fresh(Card::sidebar_default());
        let seed = model.view();

        assert!(
            model.update(sample(0.5)).is_empty(),
            "no effects are emitted"
        );
        let after = model.view();
        assert_ne!(after, seed, "the first sample must change the card");

        // The scope's phosphor is shell-owned, so an identical batch is an
        // identical tree — the dedup case the SDK documents.
        let mut twin = fresh(Card::sidebar_default());
        let _ = twin.update(sample(0.5));
        assert_eq!(twin.view(), after, "identical input, identical tree");
    }

    /// The history ring is bounded by the scope's own width: an hour of ticks
    /// must not become an hour of samples on the wire.
    ///
    /// **Falsified** by dropping the `pop_front` — the batch then grows without
    /// bound and the proto's own `MAX_SCOPE_SAMPLES` clamp is the only thing
    /// left standing between this plugin and a 4 KiB-plus frame per second.
    #[test]
    fn the_history_ring_is_bounded_by_the_scopes_width() {
        let mut model = fresh(Card::sidebar_default());
        for i in 0..(HISTORY_COLS * 3) {
            #[allow(clippy::cast_precision_loss)]
            let load = (i % 100) as f32 / 100.0;
            let _ = model.update(sample(load));
        }
        assert_eq!(model.ring.len(), HISTORY_COLS as usize);
    }

    /// **The drawer page's lines move with the samples** (#1252): each sample
    /// is folded into `panel::History` by `apply`, so the page's
    /// `Sparkline`s carry one more point per tick, and a cold tick carries
    /// none.
    ///
    /// **Falsified** by dropping `self.page.push(&snapshot)` from `apply`: the
    /// page's CPU line stays empty however many samples land.
    #[test]
    fn a_sample_moves_the_pages_history_lines() {
        let line_len = |model: &Stats| {
            hytte_plugin::display::testing::with_negotiated_vocab(SPARKLINE_VOCAB, || {
                let panel = model.view().panel.expect("a bar instance publishes a page");
                find_sparkline(&panel, "stats-panel-cpu-history")
            })
        };
        let (mut model, _rx) = fresh_bar(Card::bar_default());
        assert_eq!(
            line_len(&model),
            Some(0),
            "the seed page draws an empty line"
        );

        let _ = model.update(Input::App(Msg::Sampled(Box::default())));
        assert_eq!(line_len(&model), Some(0), "a cold tick is not a sample");

        for i in 1..=3 {
            let _ = model.update(sample(0.5));
            assert_eq!(line_len(&model), Some(i));
        }
    }

    /// How many samples the page's sparkline `id` carries, if it is there.
    fn find_sparkline(node: &Node, id: &str) -> Option<usize> {
        match node {
            Node::Sparkline {
                id: Some(found),
                values,
                ..
            } if found == id => Some(values.len()),
            Node::Box { children, .. }
            | Node::Row { children, .. }
            | Node::ListBox { children, .. } => children.iter().find_map(|c| find_sparkline(c, id)),
            Node::Expander {
                header, children, ..
            } => find_sparkline(header, id)
                .or_else(|| children.iter().find_map(|c| find_sparkline(c, id))),
            _ => None,
        }
    }

    /// Whether the page's Disk expander is open.
    fn disks_open(model: &Stats) -> bool {
        fn walk(node: &Node) -> Option<bool> {
            match node {
                Node::Expander { id, expanded, .. } if id == crate::panel::DISKS_EXPANDER_ID => {
                    Some(*expanded)
                }
                Node::Box { children, .. }
                | Node::Row { children, .. }
                | Node::ListBox { children, .. } => children.iter().find_map(walk),
                _ => None,
            }
        }
        walk(&model.view().panel.expect("a bar instance publishes a page"))
            .expect("the page has a Disk expander")
    }

    /// **The Disk card's expander is plugin-driven** (#1252): it starts
    /// collapsed like the native row, a click on its id opens it and a second
    /// click closes it again — with no effect emitted, since opening a section
    /// of a page the user is already on asks the shell for nothing.
    ///
    /// **Falsified** by dropping the `DISKS_EXPANDER_ID` arm from `update` (the
    /// click falls through to the no-op arm and the card never opens).
    #[test]
    fn a_click_on_the_disk_card_toggles_it() {
        let (mut model, _rx) = fresh_bar(Card::bar_default());
        assert!(!disks_open(&model), "collapsed by default, as native");
        let click = || Input::event(crate::panel::DISKS_EXPANDER_ID, EventKind::Click);
        assert!(
            model.update(click()).is_empty(),
            "no effect for a page-local toggle"
        );
        assert!(disks_open(&model));
        assert!(model.update(click()).is_empty());
        assert!(!disks_open(&model));
    }

    /// Whether the page's expander `id` is open, and its header's summary (the
    /// last text in it: the `name · value` or the `—`).
    fn list_state(model: &Stats, want: &str) -> (bool, String) {
        fn find<'a>(node: &'a Node, want: &str) -> Option<(&'a Node, bool)> {
            match node {
                Node::Expander {
                    id,
                    header,
                    expanded,
                    ..
                } if id == want => Some((header, *expanded)),
                Node::Box { children, .. }
                | Node::Row { children, .. }
                | Node::ListBox { children, .. } => children.iter().find_map(|c| find(c, want)),
                _ => None,
            }
        }
        fn last_text(node: &Node) -> Option<String> {
            match node {
                Node::Text { text, .. } | Node::Label { text, .. } => Some(text.clone()),
                Node::Row { children, .. } | Node::Box { children, .. } => {
                    children.iter().rev().find_map(last_text)
                }
                _ => None,
            }
        }
        let panel = model.view().panel.expect("a bar instance publishes a page");
        let (header, open) = find(&panel, want).expect("the page has the list");
        (open, last_text(header).expect("a summary"))
    }

    /// What a walk of `/proc` delivers, as the task would post it.
    fn walked() -> Input<Msg> {
        Input::App(Msg::TopApps(TopApps {
            by_cpu: vec![crate::top_apps::app("firefox", 0.5, 1 << 30)],
            by_mem: vec![crate::top_apps::app("firefox", 0.5, 1 << 30)],
        }))
    }

    /// Everything waiting on the lane.
    fn drain(rx: &mut CmdReceiver<Cmd>) -> Vec<Cmd> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    /// **The Top apps lists are plugin-driven, and gate nothing** (#1419 item
    /// 3, #1427): each starts collapsed, a click on its id flips it with no
    /// effect emitted and nothing on the lane — the walker follows the page,
    /// not the lists.
    ///
    /// **Falsified** by dropping either list's arm from `update` (the click
    /// falls through to the no-op arm), or by a list click sending a command.
    #[test]
    fn a_click_on_a_top_apps_list_toggles_it_and_nothing_else() {
        let (mut model, mut rx) = fresh_bar(Card::bar_default());
        assert_eq!(
            drain(&mut rx),
            vec![Cmd::SetVisible(true)],
            "the bar's own seed"
        );
        let cpu = crate::panel::TOP_APPS_CPU_ID;
        let ram = crate::panel::TOP_APPS_RAM_ID;
        assert!(!list_state(&model, cpu).0);
        assert!(!list_state(&model, ram).0);

        let click = |id: &str| Input::event(id, EventKind::Click);
        for (id, open) in [(cpu, true), (ram, true), (cpu, false), (ram, false)] {
            assert!(model.update(click(id)).is_empty(), "a page-local toggle");
            assert_eq!(list_state(&model, id).0, open, "{id}");
        }
        assert_eq!(drain(&mut rx), Vec::new(), "a list click gates nothing");
    }

    /// **Nothing but the page opens the walker.** A chip click, the Disk
    /// card's expander, both Top apps lists and any number of surface pushes
    /// leave the walker's switch alone — the "not for the bar chips, not for
    /// a list" half of the gate, at the reducer.
    ///
    /// **Falsified** by opening the walker from the chip-click arm, from a
    /// list's arm (the #1426 gate), or on the `init` seed.
    #[test]
    fn nothing_but_the_page_opens_the_walker() {
        let (mut model, mut rx) = fresh_bar(Card::bar_default());
        let _ = model.update(sample(0.5));
        for input in [
            Input::event(crate::card::chip_button_id("cpu"), EventKind::Click),
            Input::event(crate::panel::DISKS_EXPANDER_ID, EventKind::Click),
            Input::event(crate::panel::TOP_APPS_CPU_ID, EventKind::Click),
            Input::event(crate::panel::TOP_APPS_RAM_ID, EventKind::Click),
            Input::SlotVisible(true),
            Input::SlotVisible(false),
            Input::event(crate::panel::DISKS_EXPANDER_ID, EventKind::Click),
        ] {
            let _ = model.update(input);
        }
        assert!(
            !drain(&mut rx)
                .iter()
                .any(|cmd| matches!(cmd, Cmd::PageVisible(_))),
            "no walker switch without the page",
        );
    }

    /// **Each instance forwards its own page's visibility to its own walker,
    /// and to nothing else** (#1427). For both families: the hook sends
    /// exactly `PageVisible(v)` for every push — the seed and each edge — and
    /// never a `SetVisible`, so a page close cannot park the chips' sampler.
    /// No effects either.
    ///
    /// The sidebar instance publishes no page today, so in a live session it
    /// is only ever told `false`; it forwards that like the bar does.
    ///
    /// **Falsified** by deleting `Stats::page_visible` (the SDK's default
    /// does nothing: the lane stays empty), by forwarding a `SetVisible`
    /// alongside or instead, by sending only on a change (the repeated `false`
    /// below is the seed a sidebar instance gets), and by gating the forward
    /// on the family.
    #[test]
    fn each_instance_forwards_its_own_page_to_its_own_walker() {
        for family in [Family::Bar, Family::Sidebar] {
            let (tx, mut rx) = hytte_plugin::cmd_channel();
            let cfg = match family {
                Family::Bar => Card::bar_default(),
                Family::Sidebar => Card::sidebar_default(),
            };
            let mut model = Stats::with_config(family, cfg, tx);
            let _ = drain(&mut rx);
            for push in [false, false, true, false, true] {
                assert!(
                    Plugin::page_visible(&mut model, push).is_empty(),
                    "{family:?}: no effect",
                );
                assert_eq!(
                    drain(&mut rx),
                    vec![Cmd::PageVisible(push)],
                    "{family:?}: page_visible({push}) reaches the walker's switch, alone",
                );
            }
        }
    }

    /// **A walk lands only while the page is open, and a page close clears
    /// the lists** — so a reopened page never shows a reading taken before
    /// the close, and the one walk that was already in flight when the page
    /// closed (the gate never cancels one) is dropped when it lands. The
    /// seed may arrive before or after a sample; neither order matters.
    ///
    /// **Falsified** by dropping the `page_open` guard on `Msg::TopApps` (the
    /// late walk lands on a shut page), by dropping the clear in
    /// `page_visible` (the reopened header keeps the old name), and by never
    /// setting `page_open` (no walk ever lands).
    #[test]
    fn a_walk_lands_only_while_the_page_is_open() {
        let (mut model, _rx) = fresh_bar(Card::bar_default());
        let cpu = crate::panel::TOP_APPS_CPU_ID;
        let dash = "\u{2014}".to_owned();
        let summary = "firefox · 50%".to_owned();

        // Before the page's seed, a stray walk is not drawn — whether a
        // sample came first or not.
        let _ = model.update(walked());
        assert_eq!(list_state(&model, cpu), (false, dash.clone()));
        let _ = model.update(sample(0.5));
        let _ = model.update(walked());
        assert_eq!(list_state(&model, cpu), (false, dash.clone()));

        // The seed says the page is up: the next walk is drawn.
        let _ = Plugin::page_visible(&mut model, true);
        let _ = model.update(walked());
        assert_eq!(list_state(&model, cpu), (false, summary.clone()));

        // Closing the page clears the reading…
        let _ = Plugin::page_visible(&mut model, false);
        assert_eq!(list_state(&model, cpu), (false, dash.clone()));
        // …the walk already in flight is dropped when it lands…
        let _ = model.update(walked());
        assert_eq!(list_state(&model, cpu), (false, dash.clone()));
        // …and a reopen shows nothing until a walk after it lands.
        let _ = Plugin::page_visible(&mut model, true);
        assert_eq!(list_state(&model, cpu), (false, dash));
        let _ = model.update(walked());
        assert_eq!(list_state(&model, cpu), (false, summary));
    }

    /// **A collapsed list keeps its header summary while the page is open** —
    /// native's `name · value` beside the chevron, restored now that the
    /// walker follows the page. Opening or collapsing either list changes its
    /// flag and nothing else: the reading stays.
    ///
    /// **Falsified** by gating the walk on a list again (`Msg::TopApps`
    /// dropped while both are collapsed: the header reads `—`), and by
    /// clearing the lists on a list click.
    #[test]
    fn a_collapsed_list_keeps_its_header_summary_while_the_page_is_open() {
        let (mut model, _rx) = fresh_bar(Card::bar_default());
        let cpu = crate::panel::TOP_APPS_CPU_ID;
        let ram = crate::panel::TOP_APPS_RAM_ID;
        let _ = Plugin::page_visible(&mut model, true);
        let _ = model.update(walked());
        assert_eq!(
            list_state(&model, cpu),
            (false, "firefox · 50%".to_owned()),
            "collapsed, with the heaviest app beside the chevron",
        );
        assert_eq!(
            list_state(&model, ram),
            (false, "firefox · 1.0 GiB".to_owned())
        );

        let click = |id: &str| Input::event(id, EventKind::Click);
        for id in [cpu, ram, cpu, ram] {
            let _ = model.update(click(id));
        }
        assert_eq!(list_state(&model, cpu), (false, "firefox · 50%".to_owned()));
        assert_eq!(
            list_state(&model, ram),
            (false, "firefox · 1.0 GiB".to_owned())
        );
    }

    /// **End to end, a bar instance's page push runs the walker and nothing
    /// else does**: the real reducer and the real tasks (`sample::spawn_with`),
    /// over a counting sensors fake and a real `Walker` on a `/proc` stand-in.
    ///
    /// The page opens: a cold walk at once and a warm one a cadence later, and
    /// the collapsed CPU header then reads the share. The page closes: not one
    /// more walk in ten seconds, the header is cleared, and the chips' sampler
    /// keeps its 1 s cadence the whole time.
    ///
    /// The clock is the paused runtime's auto-advance, which tokio holds while
    /// any `spawn_blocking` task is pending, so every count here is exact (see
    /// `crate::sample`'s `the_walker_walks_on_its_own_two_second_cadence`).
    ///
    /// **Falsified** by deleting `Stats::page_visible` (no walk at all), by
    /// routing `PageVisible` to the sensors' lane, and by forwarding a page
    /// close as a `SetVisible(false)` (the chips stop).
    #[allow(clippy::float_cmp)]
    #[tokio::test(start_paused = true)]
    async fn the_page_push_runs_the_walker_end_to_end_and_the_chips_never_stop() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        use crate::sample::{Sample, spawn_with};
        use crate::top_apps::{Walker, fake_proc};

        /// The chips' sampler: reads nothing, counts its ticks.
        struct Chips(Arc<AtomicUsize>);
        impl Sample for Chips {
            type Reading = Snapshot;
            fn tick(&mut self) -> Snapshot {
                self.0.fetch_add(1, Ordering::SeqCst);
                Snapshot::default()
            }
            fn reset(&mut self) {}
        }

        let chips = Arc::new(AtomicUsize::new(0));
        let (walk, handed) = fake_proc();
        let mut walk = Some(walk);
        let (cmd_tx, cmd_rx) = hytte_plugin::cmd_channel();
        let (msg_tx, mut msg_rx) = hytte_plugin::cmd_channel();
        let mut model = Stats::with_config(Family::Bar, Card::bar_default(), cmd_tx);
        let counted = Arc::clone(&chips);
        let tasks = spawn_with(
            cmd_rx,
            msg_tx,
            Duration::from_secs(1),
            move || Chips(Arc::clone(&counted)),
            move || Walker::over(walk.take().expect("the walker is built once")),
        );
        let walks = || handed.lock().expect("not poisoned").len();
        let mut fold = |model: &mut Stats| {
            while let Ok(msg) = msg_rx.try_recv() {
                let _ = model.update(Input::App(msg));
            }
        };
        let cpu = crate::panel::TOP_APPS_CPU_ID;

        // The chips run before the page is ever open; the walker does not.
        tokio::time::sleep(Duration::from_millis(3_500)).await;
        fold(&mut model);
        assert_eq!(chips.load(Ordering::SeqCst), 4, "the chips at 0, 1, 2, 3 s");
        assert_eq!(walks(), 0, "a hidden page walks nothing");

        // The host's seed: the page is up. (Every wake-up below is off the
        // whole second, so no count depends on which of two simultaneous
        // timers the runtime polls first.)
        let _ = Plugin::page_visible(&mut model, true);
        tokio::time::sleep(Duration::from_millis(2_250)).await;
        fold(&mut model);
        assert_eq!(
            walks(),
            2,
            "a cold walk on the open edge, a warm one 2 s later"
        );
        assert_eq!(
            list_state(&model, cpu),
            (false, "firefox · 25%".to_owned()),
            "the collapsed header reads the measured share",
        );

        // The page closes; the chips must not notice.
        let chips_at = chips.load(Ordering::SeqCst);
        let _ = Plugin::page_visible(&mut model, false);
        tokio::time::sleep(Duration::from_secs(10)).await;
        fold(&mut model);
        assert_eq!(walks(), 2, "a shut page walks nothing more");
        assert_eq!(list_state(&model, cpu), (false, "\u{2014}".to_owned()));
        assert_eq!(
            chips.load(Ordering::SeqCst) - chips_at,
            10,
            "the chips kept their 1 s cadence through the page close",
        );

        drop(model);
        for task in tasks {
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .expect("every task ends once the model's lane closes")
                .expect("and not by panicking");
        }
    }

    /// **A withheld reading is not a sample**: the cold tick, whose `cpu` is
    /// `None` and whose `per_core` is empty, leaves the trace alone rather than
    /// stamping a fake rest value on it — and the card still reads as dashes
    /// afterwards, exactly as the seed render did (#1277 MEDIUM 3 / LOW 5).
    ///
    /// **Falsified** by pushing `trace_sample(cpu.unwrap_or(0.0))`
    /// unconditionally: the ring grows and the scope's first sweep starts from
    /// a bottom-rail point nothing measured.
    #[test]
    fn a_withheld_reading_leaves_the_trace_alone() {
        let mut model = fresh(Card::sidebar_default());
        let seed = model.view();

        let cold = Input::App(Msg::Sampled(Box::new(Snapshot {
            cpu: None,
            per_core: Vec::new(),
            cpu_temp_c: Some(44.0),
            ..Snapshot::default()
        })));
        assert!(model.update(cold).is_empty());
        assert!(
            model.ring.is_empty(),
            "a tick with no delta contributes no history point",
        );

        // The temperature it *did* read is a real reading and does move the
        // card, so this is not "the update was dropped".
        assert_ne!(
            model.view(),
            seed,
            "the temperature it did read still lands"
        );

        // …and the first real sample is the ring's first point.
        let _ = model.update(sample(0.5));
        assert_eq!(model.ring.len(), 1);
    }

    /// A visibility push is forwarded to the sampler and changes nothing on
    /// screen — the card keeps its last reading while the sidebar is closed
    /// rather than blanking.
    #[test]
    fn a_visibility_push_parks_the_sampler_without_touching_the_view() {
        let mut model = fresh(Card::sidebar_default());
        let _ = model.update(sample(0.25));
        let before = model.view();
        assert!(model.update(Input::SlotVisible(false)).is_empty());
        assert_eq!(model.view(), before);
        assert!(model.update(Input::SlotVisible(true)).is_empty());
        assert_eq!(model.view(), before);
    }

    /// Every other host push is a no-op, and none of them can panic — the card
    /// subscribes to one key and must survive a host that sends more.
    #[test]
    fn an_unsubscribed_push_changes_nothing() {
        let mut model = fresh(Card::sidebar_default());
        let _ = model.update(sample(0.25));
        let before = model.view();
        for input in [
            Input::Snapshot(hytte_plugin::proto::StateSnapshot::default()),
            Input::SessionLocked(true),
            Input::event("not-ours", hytte_plugin::proto::EventKind::Click),
        ] {
            assert!(model.update(input).is_empty());
            assert_eq!(model.view(), before);
        }
    }

    /// A snapshot whose every reading is absent renders without panicking —
    /// the "unavailable sensor is a dash" rule, exercised through the real
    /// `update`/`view` pair rather than through the formatters alone.
    #[test]
    fn a_machine_with_no_sensors_at_all_still_renders() {
        let mut model = fresh(Card::sidebar_default());
        let _ = model.update(Input::App(Msg::Sampled(Box::default())));
        let view = model.view();
        assert!(view.panel.is_none(), "P1 ships no drawer panel");
        // …and the frame it produces is valid on the wire.
        let render = PluginMsg::Render {
            tree: view.tree,
            panel: None,
            hidden_on: view.hidden_on,
            effects: Vec::new(),
        };
        let back: PluginMsg = decode(&encode(&render)).expect("render frame decodes");
        assert_eq!(render, back);
    }

    /// The frames built from this plugin's data are valid on the wire — the
    /// `Register` manifest as well as a populated render.
    #[test]
    fn register_and_render_frames_round_trip() {
        let reg = PluginMsg::Register {
            manifest: Stats::manifest(),
        };
        let back: PluginMsg = decode(&encode(&reg)).expect("register frame decodes");
        assert_eq!(reg, back);

        let mut model = fresh(Card::sidebar_default());
        let _ = model.update(sample(0.75));
        let view = model.view();
        let render = PluginMsg::Render {
            tree: view.tree,
            panel: view.panel.map(Box::new),
            hidden_on: view.hidden_on,
            effects: Vec::new(),
        };
        let back: PluginMsg = decode(&encode(&render)).expect("render frame decodes");
        assert_eq!(render, back);
    }

    // ── settings(): the real process environment, not just settings_from ────
    //
    // #1327: `the_launch_mount_decides_which_table_this_instance_reads` (above)
    // and `all_nine_mounts_resolve_to_a_table` pin `settings_from` — the pure
    // half — thoroughly, with an injected `lookup`. But `settings()` is the
    // *one* production call site that hands it the real environment
    // (`&|key| std::env::var(key).ok()`), and nothing in this file exercises
    // that line: replacing it with `&|_| None` leaves every test above green,
    // because none of them ever calls `settings()` itself. That is the
    // seam-wrapper hole in its third instance in this tree —
    // `hytte-plugin::runtime`'s `the_mount_env_var_reaches_the_register_frame`
    // and `hytte-claude-bridge::plugin`'s
    // `init_reaches_the_view_with_a_real_process_environment` are the first
    // two, and this follows their exact shape.

    /// Set (to any value) only on the re-exec'd child that actually runs
    /// [`settings_reads_the_real_process_environment_inner`] — the same marker
    /// shape as the two precedents named above, and for the same reason: an
    /// ordinary `cargo test` run discovers the inner test like any other and
    /// must not try to run its scenario with no launch environment set up.
    const SETTINGS_ENV_CHILD: &str = "HYTTE_PLUGIN_STATS_SETTINGS_TEST_CHILD";

    /// Printed by the child only once its scenario has run to completion and
    /// passed, so the parent can tell "the scenario passed" from "the
    /// `--exact` filter matched no test and libtest still reports `0 passed`,
    /// exit 0" — the failure mode a renamed inner test produces.
    const SETTINGS_ENV_CHILD_OK: &str = "settings-env-child-reached-the-end";

    /// **`settings()`'s own wiring to the real process environment.**
    ///
    /// `std::env::set_var` is `unsafe` in edition 2024 and this workspace
    /// forbids `unsafe_code` outright, so no in-process test can set
    /// `HYTTE_PLUGIN_MOUNT` for itself to drive a real `settings()` call. Same
    /// constraint the two precedents hit, same fix: re-exec this test binary
    /// (`std::env::current_exe`), filtered to exactly one inner test, with the
    /// variable set on the **child** via the safe `Command::env` builder.
    ///
    /// The override is `BarRight`, not [`DEFAULT_MOUNT`]'s own
    /// `SidebarRightTop`: `DEFAULT_MOUNT` is already a sidebar mount, so an
    /// override that merely repeats it could not tell "the real environment
    /// was read" from "the manifest's own default happened to agree" — the
    /// same failure mode `all_nine_mounts_resolve_to_a_table`'s doc comment
    /// above calls out for asserting a selection against itself. `BarRight` is
    /// a different family, so a neutered `settings()` (falling back to
    /// `DEFAULT_MOUNT`, i.e. `Family::Sidebar`) and a working one
    /// (`Family::Bar`) give different, checkable answers.
    ///
    /// The child's `XDG_CONFIG_HOME`/`XDG_CONFIG_DIRS` point at an empty
    /// scratch directory rather than being left to inherit the real ones —
    /// tests must not resolve `stats.toml` against the user's actual
    /// `$XDG_CONFIG_HOME` (#1101) — so `settings()`'s `config::load()` half
    /// reads only the built-in default, same on every run.
    ///
    /// Falsification: replace `&|key| std::env::var(key).ok()` with `&|_|
    /// None` inside `settings()` — the child then resolves `Family::Sidebar`
    /// from `DEFAULT_MOUNT` regardless of the real `HYTTE_PLUGIN_MOUNT=BarRight`,
    /// and its own assertion reds.
    #[test]
    fn settings_reads_the_real_process_environment() {
        let inner = "plugin::tests::settings_reads_the_real_process_environment_inner";
        let args = ["--exact", "--nocapture", "--test-threads=1", inner];
        assert!(
            args.contains(&"--exact"),
            "the re-exec must stay filtered to exactly one inner test",
        );
        let exe = std::env::current_exe().expect("this test binary's own path");
        let xdg_dir = tempfile::tempdir().expect("an empty scratch XDG directory");

        let out = std::process::Command::new(&exe)
            .args(args)
            .env(SETTINGS_ENV_CHILD, "1")
            .env("HYTTE_PLUGIN_MOUNT", "BarRight")
            .env("XDG_CONFIG_HOME", xdg_dir.path())
            .env("XDG_CONFIG_DIRS", xdg_dir.path())
            .output()
            .expect("re-exec this test binary with HYTTE_PLUGIN_MOUNT set");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "the child scenario failed ({:?})\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
            out.status,
        );
        assert!(
            stdout.contains(SETTINGS_ENV_CHILD_OK),
            "the child exited 0 without reaching the end of {inner} — a stale filter \
             matches no test and libtest still reports success\n\
             --- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        );
    }

    /// The scenario body of [`settings_reads_the_real_process_environment`].
    /// Does nothing at all unless the parent's marker is set, so an ordinary
    /// `cargo test` run — which discovers it like any other test — does not
    /// try to run it with no launch environment set up for it.
    #[test]
    fn settings_reads_the_real_process_environment_inner() {
        if std::env::var_os(SETTINGS_ENV_CHILD).is_none() {
            return;
        }
        // The precedent's self-agreement guard (#1327 review LOW 1):
        // `hytte-plugin::runtime`'s `the_mount_env_var_reaches_the_register_frame_inner`
        // opens with the equivalent of this — `assert_ne!(Echo::manifest().mount, want, …)`
        // — and this copy had dropped it, replacing it with an assertion on the
        // environment variable's *value* rather than on the design premise the
        // doc comment above `settings_reads_the_real_process_environment`
        // argues: that `BarRight` differs from `DEFAULT_MOUNT`'s own family.
        // Without this, a future `DEFAULT_MOUNT` move to a bar region — not a
        // hypothetical on a plugin whose whole point is running on both
        // families — would make a fully neutered `settings()` and a working
        // one agree, and this test would stop catching it.
        assert_ne!(
            Family::of(DEFAULT_MOUNT),
            Family::Bar,
            "test setup: the override's family must differ from DEFAULT_MOUNT's own, \
             or a neutered settings() and a working one give the same answer",
        );
        assert_eq!(
            std::env::var("HYTTE_PLUGIN_MOUNT").as_deref(),
            Ok("BarRight"),
            "test setup: the parent sets a real bar-family override",
        );
        assert_eq!(
            super::settings().family,
            Family::Bar,
            "settings() must resolve the family from the REAL process \
             environment (HYTTE_PLUGIN_MOUNT), not silently ignore it (#1327)",
        );
        println!("{SETTINGS_ENV_CHILD_OK}");
    }
}
