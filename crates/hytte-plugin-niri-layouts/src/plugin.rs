//! The widget hat: a three-button bar chip of Adwaita symbolic icons, shown on
//! each screen only while **that screen's** active workspace holds more than
//! one window.
//!
//! The chip is a `Row` of three `Button`s, each holding one
//! [`Node::Icon`] whose glyph is the layout it applies. #1026 drew those three
//! with the preem kit — Annika's answer to #1019's third question was "preem",
//! and it was a misread of what she meant; her verdict on the result was
//! "ultra gonk idé typ […] Looks shit. Adwaita icons fine." (#1019,
//! 2026-09-10), so they are themed icons now. [`Layout::icon`] holds the three
//! names and says why each one.
//!
//! `Node::Icon` carries its own `tooltip` — it is one of the seven wire
//! variants that do, and the proto's own docs call an icon "the load-bearing
//! case" for a tooltip — so the legend hangs directly on the glyph. The
//! intermediate `Node::Box` #1026 needed for that is gone with the pictograms.
//!
//! # Showing and hiding — per screen (#1050)
//!
//! "Only show when more than 1 window in workspace" (#1019). No host state
//! topic carries that — [`StateKey`](hytte_plugin::proto::StateKey) knows
//! nothing about niri — so [`crate::watch`] answers it in-process off a second
//! niri connection and pushes a [`Msg::Visibility`] whenever the answer flips.
//!
//! The answer is **one verdict per output**, not one global bool. A plugin
//! renders one tree and the host mirrors it onto every monitor, so before #1050
//! the chip on screen B followed screen A's window count — which is what Annika
//! hit on glass. [`View::hidden_on`](hytte_plugin::View::hidden_on) (the host
//! arm, #1068) is the lever: same tree everywhere, hidden on the screens whose
//! active workspace is below the threshold. [`Plugin::view`] is still a two-way
//! branch, and the second branch is still [`hidden`] — see its doc for why an
//! all-screens-hidden `chip()` is *not* the same thing.
//!
//! The other half is the click: an event carries the connector of the screen it
//! came from, and [`crate::niri::apply`] lays out **that** screen's active
//! workspace. An event the host could not attribute (`output: None` — the
//! drawer panel) falls back to the focused workspace, which is what the CLI hat
//! gets too.
//!
//! **The chip starts hidden**, and stays hidden if niri is unreachable. That is
//! deliberate: the model's initial [`watch::Verdict::default`] and
//! [`crate::watch::Watch`]'s initial verdict agree, so the very first frame the
//! host renders is the same one the first event would produce, and a plugin
//! session started outside a niri session shows nothing rather than a chip
//! whose every click toasts.
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

use crate::layout::Layout;
use crate::niri::{self, SocketTransport, Transport};
use crate::watch;
use hytte_plugin::proto::{Capability, Effect, EventKind, Manifest, Mount, Node};
use hytte_plugin::{CmdReceiver, CmdSender, Input, MsgStream, Plugin, View, nodes};

/// The mount-slot key, the audit-log subject, and the stderr log prefix.
pub(crate) const PLUGIN_ID: &str = "niri-layouts";

/// The chip's root node id. **The hidden tree carries it too** — same kind,
/// same id, so the host's reconciler keeps the widget and just empties it.
const ROOT_ID: &str = "niri-layouts";

/// Every button id is this plus the layout's own token, so
/// [`layout_for_node`] is a `strip_prefix` and the two can never disagree.
const BUTTON_PREFIX: &str = "niri-layouts-";

/// Inter-button gap, in pixels. The three are `.flat` buttons carrying their own
/// libadwaita padding, so this only has to keep the glyphs from reading as one
/// run.
const BUTTON_SPACING: u16 = 2;

/// The node id of `layout`'s icon — distinct per layout so a re-render swaps
/// the `name` prop in place instead of rebuilding the image.
fn icon_id(layout: Layout) -> String {
    format!("{BUTTON_PREFIX}{}-icon", layout.id())
}

/// One click's worth of work, handed to the worker task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Cmd {
    Apply {
        layout: Layout,
        /// The screen the click came from (#1050) — the connector name the host
        /// stamped on the event. `None` = act on the focused output, which is
        /// what an unattributable event (the drawer panel) and the CLI hat both
        /// mean. Owned rather than borrowed because it crosses onto the
        /// blocking pool.
        output: Option<String>,
    },
}

/// What this plugin's own sources send back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Msg {
    /// niri refused an apply; its own text, for the one toast. A *successful*
    /// apply is visible on screen, so a toast for it would be noise.
    Failed(String),
    /// Some output's active workspace crossed the show/hide threshold
    /// (#1019/#1050). Only ever sent on a change — see
    /// [`watch::Watch::observe`].
    Visibility(watch::Verdict),
}

pub(crate) struct NiriLayouts {
    cmds: CmdSender<Cmd>,
    /// Which screens the chip belongs on, per [`watch`]. Starts at
    /// [`Default`](watch::Verdict) — hidden everywhere; see the module docs on
    /// why hidden is the right initial state.
    visibility: watch::Verdict,
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

/// One layout's glyph: a themed [`Node::Icon`] carrying its own legend.
///
/// The name is resolved against the *shell's* `GtkIconTheme`, so an unknown one
/// renders as `image-missing` rather than failing anything — which is why
/// [`Layout::icon`]'s three are pinned as literals and checked against the
/// Adwaita theme on `$XDG_DATA_DIRS` by the tests below, in the devShell and in
/// the package build's check phase (see that test for where, exactly).
///
/// No `classes`: the glyph inherits the bar's foreground, which is what every
/// other symbolic in the bar does.
fn layout_icon(layout: Layout) -> Node {
    Node::Icon {
        id: Some(icon_id(layout)),
        name: layout.icon().to_owned(),
        classes: Vec::new(),
        tooltip: Some(layout.tooltip().to_owned()),
    }
}

/// One button: an id'd [`Node::Button`] wrapping the layout's icon.
///
/// `Node::Button` carries no `tooltip` of its own, but the icon inside it does,
/// and GTK resolves a hover against the deepest widget under the pointer before
/// walking up — so hovering the glyph gives the layout's legend and hovering the
/// button's padding falls through to the row's.
///
/// `flat` is the stock libadwaita token a bar chip's inline buttons wear; a
/// plugin cannot ship CSS of its own, and the host already wraps a bar mount in
/// its own `.ts-plugin-chip`.
fn layout_button(layout: Layout) -> Node {
    Node::Button {
        id: button_id(layout),
        classes: vec!["flat".to_owned()],
        child: Box::new(layout_icon(layout)),
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

/// The tree the chip collapses to below two windows (#1019).
///
/// An empty `Row` under [`ROOT_ID`] — the *same* kind and id the chip uses, so
/// the host reconciles in place: the three buttons are removed, the widget
/// stays, and coming back is three inserts rather than a rebuild. No tooltip
/// either: `tooltip` is a mutable prop and dropping it back to `None` **clears**
/// the hover, so an invisible chip cannot keep answering one.
///
/// Known residual, and the reason this is an empty `Row` rather than a nicer
/// story: the host wraps every bar mount in its own `.ts-plugin-chip` pill, and
/// the pill's padding is drawn whether or not the plugin's tree has anything in
/// it. The buttons go, but a few pixels of translucent pill stay. Nothing in the
/// wire vocabulary lets a plugin ask for its own card to be unmounted; closing
/// that gap is a host-side change (`trollshell/src/plugins/region.rs`), outside
/// this crate.
fn hidden() -> Node {
    nodes::row(Vec::new()).id(ROOT_ID).build()
}

/// Run one queued command against `transport`, reporting only what the human
/// needs to see.
///
/// Split out of the worker task so the click → apply → toast path is testable
/// against a fake niri, with no runtime and no socket.
pub(crate) fn apply_and_report(
    transport: &mut impl Transport,
    layout: Layout,
    on_output: Option<&str>,
) -> Option<Msg> {
    match niri::apply(transport, layout, on_output) {
        Ok(0) => {
            // The one debug line the no-op case gets. stderr, which systemd
            // routes to the journal for a plugin unit and to the terminal for
            // the CLI hat. It names the screen when there was one, because
            // "nothing to do" on a two-monitor desktop is otherwise ambiguous
            // about *which* workspace was empty (#1050).
            eprintln!(
                "[{PLUGIN_ID}] {}: no tiled columns on {}, nothing to do",
                layout.id(),
                on_output.map_or_else(
                    || "the focused workspace".to_owned(),
                    |name| format!("{name}'s active workspace"),
                )
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

/// The watcher thread's end of **this session's** message lane.
///
/// Both halves of [`watch::Verdicts`] answer the same question — is the
/// receiver still there — because that is the only shutdown signal the SDK
/// offers (see [`watch`]'s lifetime docs): `send` answers it as a side effect of
/// delivering a verdict, `open` answers it while niri is quiet and there is
/// nothing to deliver.
struct VisibilityLane(hytte_plugin::tokio::sync::mpsc::UnboundedSender<Msg>);

impl watch::Verdicts for VisibilityLane {
    fn send(&mut self, verdict: watch::Verdict) -> bool {
        self.0.send(Msg::Visibility(verdict)).is_ok()
    }

    fn open(&self) -> bool {
        !self.0.is_closed()
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
    /// state at all (the SDK adds the accent subscription on its behalf), and
    /// notably none for its own visibility — there is no niri
    /// [`StateKey`](hytte_plugin::proto::StateKey) to subscribe to, which is
    /// why [`crate::watch`] exists.
    fn manifest() -> Manifest {
        let mut m = Manifest::new(PLUGIN_ID, Mount::BarRight);
        m.capabilities = vec![Capability::Notify];
        m
    }

    fn init(cmds: CmdSender<Self::Cmd>) -> Self {
        Self {
            cmds,
            visibility: watch::Verdict::default(),
        }
    }

    fn sources(mut cmds: CmdReceiver<Self::Cmd>) -> Option<MsgStream<Self::Msg>> {
        let (msg_tx, msg_rx) = hytte_plugin::tokio::sync::mpsc::unbounded_channel();

        // The visibility watcher (#1019 round 2): its own OS thread, not a
        // runtime task. The niri event-stream read is blocking std I/O that
        // parks until niri says something, which is neither an async task nor
        // the short burst `spawn_blocking`'s pool is sized for. An
        // `UnboundedSender` is `Send`, so it feeds the same message lane the
        // click worker below does.
        //
        // **It belongs to this session, not to this process** (#1038 review,
        // HIGH-2). The SDK calls `sources` from inside `session()`, and
        // `reconnect_loop` re-enters that after every host `Shutdown` — and the
        // plugin unit is `PartOf=graphical-session.target`, so `systemctl --user
        // restart trollshell` reconnects this process rather than restarting it.
        // A watcher that ran forever would therefore leave one detached thread
        // and one live niri event stream behind per shell restart. The SDK
        // offers no cancellation handle to hang the exit off, so the shutdown
        // signal is this very channel: the runtime owns it for exactly one
        // session, `VisibilityLane` reports the receiver going away, and
        // `watch::run` returns on it — within one poll tick, measured by
        // `sources_spawns_one_watcher_thread_and_it_exits_when_its_session_does`.
        let watch_tx = msg_tx.clone();
        if let Err(e) = std::thread::Builder::new()
            .name("niri-layouts-watch".to_owned())
            .spawn(move || watch::run(PLUGIN_ID, VisibilityLane(watch_tx)))
        {
            // A thread that will not start is not worth killing the session
            // over: the chip simply stays hidden, and the CLI hat is untouched.
            eprintln!("[{PLUGIN_ID}] cannot watch niri for window counts: {e}");
        }

        hytte_plugin::tokio::spawn(async move {
            while let Some(cmd) = cmds.recv().await {
                // Destructured on its own line, not folded into the `while let`
                // pattern: a second `Cmd` variant must be a compile error here,
                // where `while let Some(Cmd::Apply { .. })` would instead treat
                // it as a non-match and silently end the worker for the session.
                let Cmd::Apply { layout, output } = cmd;
                // `Socket::send` is blocking std I/O, so it goes to the blocking
                // pool rather than stalling the SDK's current-thread runtime
                // (which is also servicing the host socket).
                let outcome = hytte_plugin::tokio::task::spawn_blocking(move || {
                    apply_and_report(&mut SocketTransport, layout, output.as_deref())
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
            // `output` is the screen this click came from (#1050) — carried
            // straight down to the worker, which hands it to `niri::apply` so
            // the layout lands on *that* monitor's active workspace. `None`
            // (the drawer panel) keeps the pre-#1050 focused-output behaviour.
            Input::Event {
                node, kind, output, ..
            } => {
                if matches!(kind, EventKind::Click)
                    && let Some(layout) = layout_for_node(&node)
                {
                    // Err only once the session is tearing down, in which case
                    // the worker is gone and there is nothing to apply to.
                    let _ = self.cmds.send(Cmd::Apply { layout, output });
                }
                Vec::new()
            }
            Input::App(Msg::Failed(error)) => vec![failure_toast(error)],
            // The whole of the show/hide rule, per screen: fold it into the
            // model and let `view` project it. No effect — the host re-renders
            // off the returned tree.
            Input::App(Msg::Visibility(verdict)) => {
                self.visibility = verdict;
                Vec::new()
            }
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

    /// The chip, hidden on the screens that do not want it (#1050) — or the
    /// collapsed tree, when no screen does.
    ///
    /// Two branches rather than "always `chip()`, hide it on the screens below
    /// the threshold", although the second reads simpler. The host's #1042
    /// region-collapse rule keys off a card that renders **nothing**, and that
    /// is what removes the `.ts-plugin-chip` pill's padding and its bar-group
    /// `spacing` gap. A `chip()` with every attached connector in `hidden_on`
    /// is invisible on each screen but is not an empty tree, so the shell would
    /// keep a few pixels of translucent pill everywhere — exactly the residual
    /// [`hidden`] documents, and the regression #1042 fixed. So: while any
    /// screen shows the chip, `hidden_on` hides it on the rest; when none does,
    /// the plugin collapses the way it always did.
    ///
    /// The `hidden_on` list is deliberately **not** carried on the collapsed
    /// branch: an empty tree is already invisible everywhere, and naming
    /// screens on it would put bytes on the wire that decide nothing.
    fn view(&self) -> View {
        if self.visibility.shows_anywhere {
            View::new(chip()).hidden_on(self.visibility.hidden_on.clone())
        } else {
            hidden().into()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Cmd, Msg, NiriLayouts, VisibilityLane, apply_and_report, button_id, chip, hidden, icon_id,
        layout_for_node,
    };
    use crate::layout::Layout;
    use crate::niri::fake::Fake;
    use crate::watch::{self, Verdicts};
    use hytte_plugin::proto::{Capability, Effect, EventKind, Mount, Node};
    use hytte_plugin::{CmdReceiver, Input, Plugin, cmd_channel};
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    /// The chip's three buttons, in render order, as `(button id, icon node)`.
    fn buttons() -> Vec<(String, Node)> {
        let Node::Row { children, .. } = chip() else {
            panic!("the chip is a Row");
        };
        children
            .into_iter()
            .map(|child| {
                let Node::Button { id, child, .. } = child else {
                    panic!("every chip child is a Button");
                };
                (id, *child)
            })
            .collect()
    }

    /// The `(id, name, tooltip)` of one button's icon.
    fn icon(node: &Node) -> (Option<&str>, &str, Option<&str>) {
        let Node::Icon {
            id, name, tooltip, ..
        } = node
        else {
            panic!("a button's child is a themed Icon, got {node:?}");
        };
        (id.as_deref(), name.as_str(), tooltip.as_deref())
    }

    /// A verdict with the chip up, hidden on `hidden_on` (#1050).
    fn showing(hidden_on: &[&str]) -> watch::Verdict {
        watch::Verdict {
            hidden_on: hidden_on.iter().map(|s| (*s).to_owned()).collect(),
            shows_anywhere: true,
        }
    }

    /// The verdict for "no screen wants the chip" — [`watch::Verdict`]'s
    /// default, and the model's own initial state.
    fn nowhere() -> watch::Verdict {
        watch::Verdict::default()
    }

    /// A plugin whose visibility watcher has already said "yes, on every
    /// attached screen".
    fn shown() -> (NiriLayouts, CmdReceiver<Cmd>) {
        let (tx, rx) = cmd_channel();
        let mut plugin = NiriLayouts::init(tx);
        plugin.update(Input::App(Msg::Visibility(showing(&[]))));
        (plugin, rx)
    }

    fn drain(cmds: &mut CmdReceiver<Cmd>) -> Vec<Cmd> {
        let mut out = Vec::new();
        while let Ok(cmd) = cmds.try_recv() {
            out.push(cmd);
        }
        out
    }

    /// Every `*.svg` basename under `dir`, recursively (bounded — an icon theme
    /// is `theme/<context>/<size>/<name>.svg`, four levels at most).
    fn svg_stems_under(dir: &Path, depth: usize, out: &mut HashSet<String>) {
        if depth == 0 {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                svg_stems_under(&path, depth - 1, out);
            } else if path.extension().is_some_and(|e| e == "svg")
                && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            {
                out.insert(stem.to_owned());
            }
        }
    }

    /// Every icon name the Adwaita themes on `$XDG_DATA_DIRS` actually ship, or
    /// `None` where no Adwaita theme is on the search path at all.
    fn adwaita_icon_names() -> Option<HashSet<String>> {
        let dirs = std::env::var_os("XDG_DATA_DIRS")?;
        let mut themes: Vec<PathBuf> = Vec::new();
        for dir in std::env::split_paths(&dirs) {
            let theme = dir.join("icons").join("Adwaita");
            if theme.is_dir() {
                themes.push(theme);
            }
        }
        if themes.is_empty() {
            return None;
        }
        let mut names = HashSet::new();
        for theme in themes {
            svg_stems_under(&theme, 4, &mut names);
        }
        Some(names)
    }

    #[test]
    fn the_chip_is_three_icon_buttons_in_layout_order() {
        let buttons = buttons();

        assert_eq!(buttons.len(), 3, "three inline glyph buttons (#1019 Q2)");
        for ((id, node), layout) in buttons.iter().zip(Layout::ALL) {
            assert_eq!(*id, button_id(layout), "click target");
            let (icon_node_id, name, tooltip) = icon(node);
            assert_eq!(icon_node_id, Some(icon_id(layout).as_str()));
            assert_eq!(name, layout.icon(), "the glyph this layout wears");
            assert_eq!(
                tooltip,
                Some(layout.tooltip()),
                "the icon's only words — and `Node::Icon` carries them itself, \
                 which is why there is no wrapper Box any more"
            );
        }
    }

    /// The three names, resolved against the **Adwaita theme actually on the
    /// search path** rather than against a list this crate also wrote.
    ///
    /// A name the theme has never heard of does not fail anything at runtime: it
    /// renders as `image-missing`, silently, on Annika's bar. This is the only
    /// gate between a typo and that.
    ///
    /// **Where it actually gates**, corrected twice now (#1038 review MED-4,
    /// then #1053's own review LOW-5 on this same paragraph): the devShell,
    /// whose `XDG_DATA_DIRS` carries the theme by hand (`nix/devshell.nix`);
    /// the package build's check phase, whose `preCheck` exports the same
    /// thing (`nix/package.nix`) — so every `nix build .#trollshell` (and
    /// every package slice, and `nix flake check`, which builds them) runs it
    /// for real; and, since this PR, `checks.system-tests`'s own `preCheck`
    /// (`flake.nix`), which previously overrode `package.nix`'s and did not
    /// inherit the export, so this test silently skipped there even though it
    /// failed loudly in the package build. nixpkgs puts **no** icon theme on a
    /// build's `XDG_DATA_DIRS` of its own accord, which is why all three paths
    /// needed their own export rather than inheriting one.
    ///
    /// A skip is indistinguishable from a pass in captured output, so the build
    /// that means this to gate says so with `TROLLSHELL_REQUIRE_ICON_THEME=1`
    /// and a missing theme then **fails** rather than skips. Without it (a bare
    /// `cargo test` outside the devShell) it still skips: failing a run that
    /// could never have answered the question helps nobody.
    #[test]
    fn every_icon_name_exists_in_the_adwaita_theme_on_the_search_path() {
        let required =
            std::env::var_os("TROLLSHELL_REQUIRE_ICON_THEME").is_some_and(|want| want == "1");
        let Some(names) = adwaita_icon_names() else {
            assert!(
                !required,
                "TROLLSHELL_REQUIRE_ICON_THEME=1, but no icons/Adwaita is on \
                 $XDG_DATA_DIRS ({:?}) — the build that set that meant to check \
                 the three icon names for real, so skipping here is itself the \
                 bug",
                std::env::var_os("XDG_DATA_DIRS")
            );
            eprintln!(
                "SKIPPED: no icons/Adwaita on $XDG_DATA_DIRS — run this inside \
                 the devShell to check the three icon names for real"
            );
            return;
        };
        assert!(
            names.len() > 100,
            "found an Adwaita theme with only {} icons — that is not the real \
             theme, and this test would pass on anything",
            names.len()
        );
        for layout in Layout::ALL {
            assert!(
                names.contains(layout.icon()),
                "{} wears '{}', which the Adwaita theme on $XDG_DATA_DIRS does \
                 not ship — it would render as image-missing",
                layout.id(),
                layout.icon()
            );
        }
    }

    #[test]
    fn the_three_glyphs_are_distinct() {
        let names: HashSet<String> = buttons()
            .iter()
            .map(|(_, node)| icon(node).1.to_owned())
            .collect();

        assert_eq!(names.len(), 3, "two layouts drew the same glyph: {names:?}");
    }

    /// Nothing about the chip is preem any more (#1019 round 2). Stated
    /// negatively as well as positively: the shape assertions above would still
    /// pass if a *fourth* child appeared holding a rasterised panel.
    #[test]
    fn no_part_of_the_chip_is_a_rasterised_buffer() {
        let Node::Row { children, .. } = chip() else {
            panic!("the chip is a Row");
        };
        for child in &children {
            assert!(
                matches!(child, Node::Button { .. }),
                "the chip holds buttons and nothing else: {child:?}"
            );
            let Node::Button { child, .. } = child else {
                unreachable!("just asserted");
            };
            assert!(
                matches!(**child, Node::Icon { .. }),
                "a button's child is a themed icon, never a preem buffer: {child:?}"
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
        for layout in Layout::ALL {
            assert_eq!(
                layout_for_node(&icon_id(layout)),
                None,
                "the icon's own id is not a click target"
            );
        }
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
        assert!(
            m.subscribes.is_empty(),
            "no host state drives the chip — there is no niri StateKey, which \
             is exactly why `watch` exists"
        );
    }

    // ── Visibility (#1019 round 2) ───────────────────────────────────────────

    /// The default is hidden, and hidden is an *empty* tree — not a chip with
    /// invisible buttons, which would still take clicks.
    #[test]
    fn a_fresh_plugin_renders_the_hidden_tree() {
        let (tx, _rx) = cmd_channel();
        let plugin = NiriLayouts::init(tx);

        let view = plugin.view();

        assert_eq!(view.tree, hidden(), "hidden until niri says otherwise");
        assert_ne!(view.tree, chip(), "and it is emphatically not the chip");
        assert!(view.panel.is_none(), "the chip is the whole surface");
    }

    /// The hidden tree keeps the chip's kind and id so the host reconciles in
    /// place, and carries nothing else at all.
    #[test]
    fn the_hidden_tree_is_an_empty_row_under_the_chips_own_id() {
        let Node::Row {
            id,
            children,
            tooltip,
            ..
        } = hidden()
        else {
            panic!("the hidden tree is a Row, like the chip");
        };
        let Node::Row { id: chip_id, .. } = chip() else {
            panic!("the chip is a Row");
        };

        assert_eq!(id, chip_id, "same id → the host reuses the widget");
        assert!(children.is_empty(), "and holds no buttons at all");
        assert_eq!(
            tooltip, None,
            "an invisible chip must not keep answering a hover — `tooltip` is a \
             mutable prop, so None clears it"
        );
    }

    #[test]
    fn a_visible_push_reveals_the_chip_and_a_hidden_one_takes_it_back() {
        let (tx, _rx) = cmd_channel();
        let mut plugin = NiriLayouts::init(tx);

        let effects = plugin.update(Input::App(Msg::Visibility(showing(&[]))));
        assert!(effects.is_empty(), "visibility is a render, not an effect");
        let shown = plugin.view().tree;
        assert_eq!(shown, chip(), "two windows: the chip is back");

        plugin.update(Input::App(Msg::Visibility(nowhere())));
        let gone = plugin.view().tree;
        assert_eq!(gone, hidden(), "back down to one window");
        // Stated against the *other* branch, not only against `hidden()`: with
        // both sides read out of this module, a `hidden()` that quietly returned
        // the chip would satisfy every assertion above. Measured — mutating it
        // that way left this test green until this line was added.
        assert_ne!(gone, shown, "the two branches must be different trees");
        let Node::Row { children, .. } = gone else {
            panic!("still a Row");
        };
        assert!(children.is_empty(), "and the hidden one holds no buttons");
    }

    /// #1050's visibility half: one tree, hidden on the screens whose active
    /// workspace is below the threshold.
    #[test]
    fn a_mixed_verdict_renders_the_chip_hidden_on_the_quiet_screens() {
        let (tx, _rx) = cmd_channel();
        let mut plugin = NiriLayouts::init(tx);

        plugin.update(Input::App(Msg::Visibility(showing(&["DP-2"]))));

        let view = plugin.view();
        assert_eq!(
            view.tree,
            chip(),
            "the tree is the same everywhere — `hidden_on` is the only \
             per-screen lever the wire has"
        );
        assert_eq!(
            view.hidden_on,
            vec!["DP-2".to_owned()],
            "…and it names the screen whose workspace is quiet, verbatim"
        );
    }

    /// The list is projected from the model, not accumulated: a screen that
    /// filled up again leaves `hidden_on` on the very next render.
    #[test]
    fn a_screen_that_fills_up_drops_out_of_hidden_on() {
        let (tx, _rx) = cmd_channel();
        let mut plugin = NiriLayouts::init(tx);
        plugin.update(Input::App(Msg::Visibility(showing(&["DP-1", "DP-2"]))));

        plugin.update(Input::App(Msg::Visibility(showing(&["DP-2"]))));

        assert_eq!(
            plugin.view().hidden_on,
            vec!["DP-2".to_owned()],
            "DP-1 must not be left behind from the previous verdict"
        );
    }

    /// With no screen above the threshold the plugin collapses **the way it
    /// always did** — an empty tree, and no `hidden_on` at all.
    ///
    /// Not "the chip, hidden on every connector": that is invisible per screen
    /// but is not an empty tree, so the host's #1042 region-collapse rule would
    /// not fire and every screen would keep the `.ts-plugin-chip` pill's
    /// padding and its bar-group spacing gap.
    #[test]
    fn no_screen_wanting_the_chip_collapses_to_the_empty_tree() {
        let (tx, _rx) = cmd_channel();
        let mut plugin = NiriLayouts::init(tx);
        plugin.update(Input::App(Msg::Visibility(showing(&["DP-1"]))));

        plugin.update(Input::App(Msg::Visibility(nowhere())));

        let view = plugin.view();
        assert_eq!(view.tree, hidden(), "#1042's collapse rule needs this");
        assert!(
            view.hidden_on.is_empty(),
            "an empty tree is invisible everywhere on its own; naming screens \
             on it would put bytes on the wire that decide nothing: {:?}",
            view.hidden_on
        );
    }

    /// The seed frame — before the watcher has said anything — is the collapsed
    /// one, on every screen.
    #[test]
    fn the_first_frame_hides_the_chip_everywhere() {
        let (tx, _rx) = cmd_channel();
        let plugin = NiriLayouts::init(tx);

        let view = plugin.view();

        assert_eq!(view.tree, hidden());
        assert!(view.hidden_on.is_empty());
    }

    /// The chip must survive a failure toast: a niri that refuses an apply says
    /// nothing about how many windows are open.
    #[test]
    fn a_failure_toast_does_not_hide_the_chip() {
        let (mut plugin, _rx) = shown();

        plugin.update(Input::App(Msg::Failed("no such window".to_owned())));

        assert_eq!(plugin.view().tree, chip());
    }

    #[test]
    fn a_click_queues_exactly_that_layout_and_emits_no_effect() {
        let (mut plugin, mut rx) = shown();

        for layout in Layout::ALL {
            let effects = plugin.update(Input::event(button_id(layout), EventKind::Click));

            assert!(effects.is_empty(), "the work is queued, not effected");
            assert_eq!(
                drain(&mut rx),
                vec![Cmd::Apply {
                    layout,
                    output: None
                }]
            );
        }
    }

    /// #1050's click half: the connector the host stamped on the event rides
    /// down to the worker, so the layout lands on the screen that was clicked.
    #[test]
    fn a_click_carries_the_screen_it_came_from_down_to_the_worker() {
        let (mut plugin, mut rx) = shown();

        plugin.update(Input::event_on(
            button_id(Layout::Golden),
            EventKind::Click,
            Some("DP-2".to_owned()),
        ));

        assert_eq!(
            drain(&mut rx),
            vec![Cmd::Apply {
                layout: Layout::Golden,
                output: Some("DP-2".to_owned()),
            }],
            "the worker must be told which screen, or it falls back to the \
             focused one — which is #1050 itself"
        );
    }

    /// …and an event the host could not attribute (`output: None` — the drawer
    /// panel) queues the fallback, rather than a screen invented here.
    #[test]
    fn an_unattributable_click_queues_no_screen_at_all() {
        let (mut plugin, mut rx) = shown();

        plugin.update(Input::event(button_id(Layout::Equal), EventKind::Click));

        assert_eq!(
            drain(&mut rx),
            vec![Cmd::Apply {
                layout: Layout::Equal,
                output: None,
            }],
            "`None` means *not attributable*, and stays `None` all the way to \
             `niri::apply`'s focused-output fallback"
        );
    }

    #[test]
    fn a_click_on_an_unknown_node_queues_nothing() {
        let (mut plugin, mut rx) = shown();

        plugin.update(Input::event("niri-layouts", EventKind::Click));

        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn a_non_click_event_on_a_button_queues_nothing() {
        let (mut plugin, mut rx) = shown();

        plugin.update(Input::event(
            button_id(Layout::Golden),
            EventKind::Scroll { dx: 0.0, dy: 1.0 },
        ));

        assert!(drain(&mut rx).is_empty(), "only a click applies a layout");
    }

    #[test]
    fn a_worker_failure_becomes_one_notify_toast_carrying_niris_text() {
        let (mut plugin, _rx) = shown();

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
        let (mut plugin, mut rx) = shown();
        let mut niri = Fake::two_columns();
        niri.action_error = Some("no such window".to_owned());

        // 1. The click queues the layout…
        plugin.update(Input::event(button_id(Layout::Golden), EventKind::Click));
        let queued = drain(&mut rx);
        assert_eq!(
            queued,
            vec![Cmd::Apply {
                layout: Layout::Golden,
                output: None
            }]
        );

        // 2. …the worker body applies it and reports the failure…
        let Cmd::Apply { layout, output } = &queued[0];
        let report = apply_and_report(&mut niri, *layout, output.as_deref());
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

    // ── The watcher's lifetime (#1038 review, HIGH-2) ───────────────────────

    /// Poll `done` until it holds, or fail after five seconds.
    fn wait_until(mut done: impl FnMut() -> bool, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if done() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("{what}");
    }

    /// The lane the watcher thread sends on: it carries `Msg::Visibility`, and
    /// a dropped receiver — the end of a session — is reported both ways.
    #[test]
    fn the_visibility_lane_carries_verdicts_and_reports_a_dropped_receiver() {
        let (tx, mut rx) = hytte_plugin::tokio::sync::mpsc::unbounded_channel();
        let mut lane = VisibilityLane(tx);

        assert!(
            lane.send(showing(&["DP-2"])),
            "an open lane takes the verdict"
        );
        assert!(lane.open());
        assert_eq!(
            rx.try_recv().ok(),
            Some(Msg::Visibility(showing(&["DP-2"]))),
            "…and it arrives whole — the hidden-on list included, which is the \
             only thing that distinguishes one screen's answer from another's"
        );

        drop(rx);
        assert!(
            !lane.send(nowhere()),
            "a dropped receiver is the shutdown signal `watch::run` returns on"
        );
        assert!(
            !lane.open(),
            "…and it is legible without having to send anything, which is what a \
             quiet niri needs"
        );
    }

    /// `sources` starts exactly one watcher thread, and that thread **ends with
    /// its session**.
    ///
    /// Both halves matter and both were green under a mutation before this
    /// existed: deleting the spawn shipped a chip that never appears (the
    /// review's M17), and the shipped `loop {}` leaked one thread plus one live
    /// niri event stream per shell restart, since the SDK calls `sources` once
    /// per *session* and a plugin unit outlives `systemctl --user restart
    /// trollshell`.
    ///
    /// The drop below is exactly what the SDK does: `session()` owns the message
    /// stream, so it drops when the session ends.
    #[test]
    fn sources_spawns_one_watcher_thread_and_it_exits_when_its_session_does() {
        let rt = hytte_plugin::tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime for the source tasks");
        let _entered = rt.enter();
        let (cmd_tx, cmd_rx) = cmd_channel();
        let before = watch::live_watchers();

        let stream = NiriLayouts::sources(cmd_rx).expect("this plugin has sources");

        wait_until(
            || watch::live_watchers() == before + 1,
            "sources() never started the niri watcher — the chip would never appear",
        );

        drop(stream);
        drop(cmd_tx);

        wait_until(
            || watch::live_watchers() == before,
            "the watcher thread outlived the session that spawned it — one leaked \
             thread and one live niri event stream per shell restart",
        );
    }

    #[test]
    fn a_successful_apply_reports_nothing_to_the_session() {
        let mut niri = Fake::two_columns();

        assert_eq!(apply_and_report(&mut niri, Layout::Split, None), None);
        assert_eq!(
            niri.widths(),
            vec![(10, 50.0), (20, 50.0)],
            "it really did apply (at niri's percentage unit), it just has \
             nothing to say about it"
        );
    }

    #[test]
    fn an_empty_workspace_reports_nothing_either() {
        let mut niri = Fake::with(Vec::new());

        assert_eq!(
            apply_and_report(&mut niri, Layout::Equal, None),
            None,
            "a no-op gets a log line, not a toast"
        );
    }
}
