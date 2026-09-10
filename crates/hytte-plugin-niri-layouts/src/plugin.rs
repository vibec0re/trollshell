//! The widget hat: a three-button bar chip of Adwaita symbolic icons, shown
//! only while the focused workspace holds more than one window.
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
//! # Showing and hiding
//!
//! "Only show when more than 1 window in workspace" (#1019). No host state
//! topic carries that — [`StateKey`](hytte_plugin::proto::StateKey) knows
//! nothing about niri — so [`crate::watch`] answers it in-process off a second
//! niri connection and pushes a [`Msg::Visible`] whenever the answer flips.
//! [`Plugin::view`] is then a two-way branch: the chip, or [`hidden`], an empty
//! `Row` under the same root id so the host reuses the widget instead of
//! rebuilding it.
//!
//! **The chip starts hidden**, and stays hidden if niri is unreachable. That is
//! deliberate: the model's initial `visible: false` and [`crate::watch::Watch`]'s
//! initial verdict agree, so the very first frame the host renders is the same
//! one the first event would produce, and a plugin session started outside a
//! niri session shows nothing rather than a chip whose every click toasts.
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cmd {
    Apply(Layout),
}

/// What this plugin's own sources send back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Msg {
    /// niri refused an apply; its own text, for the one toast. A *successful*
    /// apply is visible on screen, so a toast for it would be noise.
    Failed(String),
    /// The focused workspace crossed the show/hide threshold (#1019). Only ever
    /// sent on a change — see [`watch::Watch::observe`].
    Visible(bool),
}

pub(crate) struct NiriLayouts {
    cmds: CmdSender<Cmd>,
    /// Whether the focused workspace holds more than one window. Starts
    /// `false`; see the module docs on why hidden is the right initial state.
    visible: bool,
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
/// Adwaita theme on `$XDG_DATA_DIRS` by the tests below.
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
            visible: false,
        }
    }

    fn sources(mut cmds: CmdReceiver<Self::Cmd>) -> Option<MsgStream<Self::Msg>> {
        let (msg_tx, msg_rx) = hytte_plugin::tokio::sync::mpsc::unbounded_channel();

        // The visibility watcher (#1019 round 2): its own OS thread, not a
        // runtime task. `Socket::read_events` hands back a *blocking* closure
        // that parks until niri says something, which is neither an async task
        // nor the short burst `spawn_blocking`'s pool is sized for. An
        // `UnboundedSender` is `Send`, so it feeds the same message lane the
        // click worker below does.
        let watch_tx = msg_tx.clone();
        if let Err(e) = std::thread::Builder::new()
            .name("niri-layouts-watch".to_owned())
            .spawn(move || {
                watch::run(PLUGIN_ID, |visible| {
                    // Err only once the session is tearing down.
                    let _ = watch_tx.send(Msg::Visible(visible));
                });
            })
        {
            // A thread that will not start is not worth killing the session
            // over: the chip simply stays hidden, and the CLI hat is untouched.
            eprintln!("[{PLUGIN_ID}] cannot watch niri for window counts: {e}");
        }

        hytte_plugin::tokio::spawn(async move {
            while let Some(cmd) = cmds.recv().await {
                // Destructured on its own line, not folded into the `while let`
                // pattern: a second `Cmd` variant must be a compile error here,
                // where `while let Some(Cmd::Apply(..))` would instead treat it
                // as a non-match and silently end the worker for the session.
                let Cmd::Apply(layout) = cmd;
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
            // The whole of the show/hide rule, host-side: fold it into the
            // model and let `view` project it. No effect — the host re-renders
            // off the returned tree.
            Input::App(Msg::Visible(visible)) => {
                self.visible = visible;
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

    fn view(&self) -> View {
        // Bound rather than folded into one expression: `if a { x } else { y }
        // .into()` binds the method call to the else-arm, not to the `if`.
        let tree = if self.visible { chip() } else { hidden() };
        tree.into()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Cmd, Msg, NiriLayouts, apply_and_report, button_id, chip, hidden, icon_id, layout_for_node,
    };
    use crate::layout::Layout;
    use crate::niri::fake::Fake;
    use hytte_plugin::proto::{Capability, Effect, EventKind, Mount, Node};
    use hytte_plugin::{CmdReceiver, Input, Plugin, cmd_channel};
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};

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

    /// A plugin whose visibility watcher has already said "yes".
    fn shown() -> (NiriLayouts, CmdReceiver<Cmd>) {
        let (tx, rx) = cmd_channel();
        let mut plugin = NiriLayouts::init(tx);
        plugin.update(Input::App(Msg::Visible(true)));
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
    /// gate between a typo and that. It is deliberately not `#[ignore]`d: it
    /// runs in the devShell and under `nix flake check`, and skips loudly
    /// wherever no theme is installed (the crane build's sandbox, say) rather
    /// than failing a build that could never have answered the question.
    #[test]
    fn every_icon_name_exists_in_the_adwaita_theme_on_the_search_path() {
        let Some(names) = adwaita_icon_names() else {
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

        let effects = plugin.update(Input::App(Msg::Visible(true)));
        assert!(effects.is_empty(), "visibility is a render, not an effect");
        let shown = plugin.view().tree;
        assert_eq!(shown, chip(), "two windows: the chip is back");

        plugin.update(Input::App(Msg::Visible(false)));
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
        let (mut plugin, mut rx) = shown();

        plugin.update(Input::Event {
            node: "niri-layouts".to_owned(),
            kind: EventKind::Click,
        });

        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn a_non_click_event_on_a_button_queues_nothing() {
        let (mut plugin, mut rx) = shown();

        plugin.update(Input::Event {
            node: button_id(Layout::Golden),
            kind: EventKind::Scroll { dx: 0.0, dy: 1.0 },
        });

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
            vec![(10, 50.0), (20, 50.0)],
            "it really did apply (at niri's percentage unit), it just has \
             nothing to say about it"
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
