//! The TEA core: manifest, model, `update`, `view`.
//!
//! Everything the host can do to this plugin arrives as an [`Input`] and
//! everything it does about it is a fold into the model plus a re-projection —
//! so the whole plugin is testable by handing [`Stats::update`] a snapshot
//! literal, which is what the tests here do. No `/proc` is read in any of them;
//! the one thing that touches the filesystem is [`crate::sample::Sampler`],
//! which lives behind the command lane in [`Stats::sources`].

use std::collections::VecDeque;
use std::sync::OnceLock;

use hytte_plugin::proto::{Capability, Effect, Manifest, Mount, StateKey};
use hytte_plugin::tokio_stream::wrappers::UnboundedReceiverStream;
use hytte_plugin::{CmdReceiver, CmdSender, Input, MsgStream, Plugin, View};

use crate::card::{self, Widgets};
use crate::config::{self, Family};
use crate::mount;
use crate::sample::{Cmd, Msg, Snapshot};

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
/// through `lookup` and the config is handed in, so every branch is testable
/// without an environment or a file.
#[must_use]
pub fn settings_from(
    manifest_mount: Mount,
    lookup: &dyn Fn(&str) -> Option<String>,
    stats: &config::Stats,
) -> Settings {
    let family = Family::of(mount::effective(manifest_mount, lookup));
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
        let resolved = settings_from(DEFAULT_MOUNT, &mount::env_lookup, &stats);
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
    /// This instance's resolved `stats.toml` table.
    cfg: config::Card,
    /// The latest sample. Defaults to the "nothing measured yet" state, which
    /// the seed render draws as dashes.
    snapshot: Snapshot,
    /// The overall-load history the scope sweeps, newest last, capped at the
    /// scope's own column count.
    ring: VecDeque<f32>,
    /// The preem widgets, held across renders so the shell keeps one renderer
    /// instance per node (and so the raster fallback keeps its animation).
    widgets: Widgets,
    /// The command lane to the sampler task: the host's visibility push,
    /// forwarded so a closed sidebar parks the poller.
    cmds: CmdSender<Cmd>,
}

impl Stats {
    /// Fold one fresh sample into the model.
    ///
    /// Split out of [`Plugin::update`] so the tests can drive it directly with
    /// a `Snapshot` literal — which is also the only way they can, since the
    /// real sampler is on the other side of a `spawn_blocking`.
    pub fn apply(&mut self, snapshot: Snapshot) {
        self.widgets.fit_cores(snapshot.per_core.len().max(1));
        self.widgets
            .set_gpu(snapshot.gpu.as_ref().and_then(|g| g.load), self.dt());

        self.ring.push_back(card::trace_sample(snapshot.cpu));
        while self.ring.len() > card::HISTORY_COLS as usize {
            self.ring.pop_front();
        }
        // `make_contiguous` is why the ring is a `VecDeque` and not a `Vec` with
        // a rotating index: the scope wants one slice, and this is the cheap
        // way to hand it one without copying on every render.
        let ring: Vec<f32> = self.ring.iter().copied().collect();
        self.widgets.push_history(&ring);

        self.snapshot = snapshot;
    }

    /// The heartbeat interval, as the raster-path animations measure it.
    fn dt(&self) -> f32 {
        self.cfg.poll.as_secs_f32()
    }

    /// A model with an explicit config — the seam every test in this module
    /// uses, so none of them touches the process environment or the real XDG
    /// search path.
    #[must_use]
    pub fn with_config(cfg: config::Card, cmds: CmdSender<Cmd>) -> Self {
        Self {
            cfg,
            snapshot: Snapshot::default(),
            ring: VecDeque::new(),
            widgets: Widgets::default(),
            cmds,
        }
    }
}

impl Plugin for Stats {
    type Msg = Msg;
    type Cmd = Cmd;

    /// Subscribes to [`StateKey::SlotVisible`] and asks for **no capabilities
    /// at all**.
    ///
    /// The subscription is not optional for this plugin: it is the only thing
    /// that lets the sampler park while the sidebar is closed, and
    /// `docs/plugin-env.md` makes it the requirement for any plugin that can be
    /// moved across mount families — which this one is built to be. A bar mount
    /// receives a constant `true` (the host treats a chip as always on screen),
    /// so the same code polls at full rate there, correctly.
    ///
    /// No capabilities, because the card asks the shell for nothing: it renders
    /// a tree and reads sensors in its own process. In particular **not**
    /// `RunCommand` — nothing here launches anything — and not `OpenPage`,
    /// which would be the capability for a drawer panel this phase does not
    /// ship (P2, #1251).
    fn manifest() -> Manifest {
        let mut m = Manifest::new(PLUGIN_ID, DEFAULT_MOUNT);
        m.subscribes = vec![StateKey::SlotVisible];
        m.capabilities = Vec::<Capability>::new();
        m
    }

    fn init(cmds: CmdSender<Self::Cmd>) -> Self {
        Self::with_config(settings().card, cmds)
    }

    /// The sampler task: one per session, owning the `/proc` reads and the
    /// visibility gate. Its messages come back as [`Msg::Sampled`].
    fn sources(cmds: CmdReceiver<Self::Cmd>) -> Option<MsgStream<Self::Msg>> {
        let (msg_tx, msg_rx) = hytte_plugin::cmd_channel::<Msg>();
        tokio::spawn(crate::sample::sampler_task(
            cmds,
            msg_tx,
            settings().card.poll,
        ));
        Some(Box::pin(UnboundedReceiverStream::new(msg_rx)))
    }

    fn update(&mut self, input: Input<Self::Msg>) -> Vec<Effect> {
        match input {
            // A fresh sample: the only thing that moves this card.
            Input::App(Msg::Sampled(snapshot)) => self.apply(*snapshot),
            // The visibility gate (#288): forward down the command lane so the
            // sampler parks and resumes. A dropped receiver means the session is
            // tearing down, which is fine to ignore.
            Input::SlotVisible(visible) => {
                let _ = self.cmds.send(Cmd::SetVisible(visible));
            }
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

    fn view(&self) -> View {
        card::card(self.cfg, &self.snapshot, &self.widgets).into()
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_MOUNT, PLUGIN_ID, Settings, Stats, settings_from};
    use crate::card::HISTORY_COLS;
    use crate::config::{self, Card, Family};
    use crate::sample::{Gpu, Msg, Snapshot};
    use hytte_plugin::proto::{Capability, Mount, PluginMsg, StateKey, decode, encode};
    use hytte_plugin::{Input, Plugin};

    fn fresh(cfg: Card) -> Stats {
        Stats::with_config(cfg, hytte_plugin::cmd_channel().0)
    }

    fn sample(cpu: f32) -> Input<Msg> {
        Input::App(Msg::Sampled(Box::new(Snapshot {
            cpu,
            per_core: vec![cpu; 4],
            cpu_temp_c: Some(50.0),
            gpu: Some(Gpu {
                name: "test".to_owned(),
                load: Some(cpu),
            }),
        })))
    }

    /// The shipped manifest: the id a single instance registers under, the
    /// right sidebar, the one subscription that makes the poll gate work, and
    /// **no capabilities**.
    ///
    /// The capability list is the security-relevant half and is asserted as
    /// *empty* rather than as "does not contain RunCommand": the host
    /// auto-grants every manifest capability, so the only safe assertion is the
    /// whole list.
    #[test]
    fn the_manifest_asks_for_the_right_sidebar_and_no_capabilities() {
        let m = Stats::manifest();
        assert_eq!(m.id, PLUGIN_ID);
        assert_eq!(m.id, "stats");
        assert_eq!(m.mount, DEFAULT_MOUNT);
        assert_eq!(m.mount, Mount::SidebarRightTop);
        assert_eq!(
            m.capabilities,
            Vec::<Capability>::new(),
            "the card asks the shell for nothing at all",
        );
        assert_eq!(
            m.subscribes,
            vec![StateKey::SlotVisible],
            "the visibility push is what parks the sampler behind a closed sidebar",
        );
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
}
