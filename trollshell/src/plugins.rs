//! Out-of-process widget-plugin **host transport** (frontend B; #35 PR 2, on
//! the #266 wire protocol and the #199 reconciler).
//!
//! The host **listens** on one same-user socket
//! (`$XDG_RUNTIME_DIR/trollshell/plugin.sock`); plugins are systemd user units
//! that dial in and self-identify with [`PluginMsg::Register`]. Supervision is
//! systemd's job — a crash is just a disconnect. This module is the transport
//! only: it does **not** spawn or supervise plugins (see the spec's topology).
//!
//! ## Topology & threading
//!
//! The UDS listener and every per-plugin read/write task run on the process-wide
//! tokio runtime (`runtime::handle()`). The reconciler, the effect broker, and
//! the clock pump run on the GTK main thread. The two sides are bridged **only**
//! through [`futures_signals::signal::Mutable`] (tokio→GTK render mailbox) and a
//! [`tokio::sync::watch`] channel (GTK→tokio clock state) — never a shared
//! `Arc<Mutex>` threaded through widgets.
//!
//! ```text
//! plugin ──Register/Render/Log/Pong──▶ per-conn reader task ──▶ render mailbox (Mutable)
//!                                                              ──▶ tracing / liveness
//! GTK reconciler ◀── render mailbox ── maps wire::Node → hytte_ui::Node
//! GTK reconciler ──on_event──▶ per-conn outbound (mpsc) ──▶ writer task ──▶ plugin (Event)
//! GTK clock pump ──▶ watch ──▶ per-conn snapshot task ──▶ writer task ──▶ plugin (StateSnapshot)
//! ```
//!
//! ## Coalescing (latest-wins)
//!
//! Both bridges coalesce, per the spec's "always accept new view state, no
//! deltas, latest-wins":
//! - The render mailbox is a `Mutable<Option<SlotRender>>`; a new frame
//!   overwrites the previous in place, and a slow GTK consumer only ever sees
//!   the newest tree (superseded frames are dropped).
//! - The clock bridge is a `watch` channel; a per-conn task reads
//!   `borrow_and_update()` so bursts collapse to the latest `ClockState`.
//!
//! ## v1 scope (see the PR body for the full deferred list)
//!
//! - **Mounts:** only the two sidebar slots ([`Mount::SidebarTop`] /
//!   [`Mount::SidebarBottom`]); bar mounts are accepted but their renders are
//!   dropped (deferred).
//! - **State:** only [`StateKey::Clock`].
//! - **Effects:** only [`Effect::OpenPage`] is brokered (mapped to the modal
//!   drawer); every other effect is logged "unsupported in v1" and skipped.
//!   Capability enforcement / audit-log / the `RunCommand` round-trip are
//!   deferred.

use std::cell::RefCell;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::rc::Rc;

use chrono::{DateTime, Local};
use hytte::futures_signals::signal::{Mutable, Signal};
use hytte::gtk::{self, glib, prelude::*};
use hytte::prelude::*;
use hytte::reactive::registry;
use hytte::services::clock;
use hytte::ui::{Dir as UiDir, EventKind as UiEventKind, Node as UiNode, NodeId, Reconciler};
use hytte_plugin_proto::{
    ClockState, Effect, HostMsg, LogLevel, Mount, Page, PluginMsg, StateKey, StateSnapshot,
    read_frame, wire, write_frame,
};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, watch};

/// The host-listens socket, under the same-user runtime dir (`0700`). Its
/// parent (`trollshell/`) is created and the socket file tightened to `0600`.
const SOCKET_DIR: &str = "trollshell";
const SOCKET_FILE: &str = "plugin.sock";

// ── Service ─────────────────────────────────────────────────────────────────

/// The plugin host transport service. Registered in `main.rs` via `App::with`.
pub struct PluginsService;

/// One rendered plugin frame parked in a mount slot's coalescing mailbox: the
/// declarative tree, its bundled effects, and a handle to send frames **back**
/// to the plugin that produced it (event round-trip). `Clone` so it can ride a
/// `Mutable` signal to the GTK reconcilers.
#[derive(Clone)]
struct SlotRender {
    plugin_id: String,
    tree: wire::Node,
    effects: Vec<Effect>,
    outbound: mpsc::UnboundedSender<HostMsg>,
}

/// Registry handles for the plugin host. The two sidebar render mailboxes are
/// written from tokio (a plugin's reader task) and read on the GTK thread (the
/// reconcilers + the effect broker). `clock_tx` is written on the GTK thread
/// (the clock pump) and subscribed from tokio (per-conn snapshot tasks).
#[doc(hidden)]
pub struct PluginHandles {
    sidebar_top: Mutable<Option<SlotRender>>,
    sidebar_bottom: Mutable<Option<SlotRender>>,
    clock_tx: watch::Sender<Option<ClockState>>,
}

/// Clones of the shared handles handed to the tokio listener + per-conn tasks.
#[derive(Clone)]
struct ListenerCtx {
    sidebar_top: Mutable<Option<SlotRender>>,
    sidebar_bottom: Mutable<Option<SlotRender>>,
    clock_rx: watch::Receiver<Option<ClockState>>,
}

impl Service for PluginsService {
    type Handles = PluginHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let (clock_tx, clock_rx) = watch::channel(None);
        let handles = PluginHandles {
            sidebar_top: Mutable::new(None),
            sidebar_bottom: Mutable::new(None),
            clock_tx,
        };
        let ctx = ListenerCtx {
            sidebar_top: handles.sidebar_top.clone(),
            sidebar_bottom: handles.sidebar_bottom.clone(),
            clock_rx,
        };
        rt.spawn(async move {
            if let Err(e) = listen(&ctx).await {
                tracing::warn!(error = %e, "plugin host listener stopped");
            }
        });
        handles
    }
}

/// Register the plugin host transport on an `App`.
#[must_use]
pub fn service() -> PluginsService {
    PluginsService
}

// ── GTK-side install: clock pump + effect broker ─────────────────────────────

/// Wire the GTK-main-thread halves of the transport: the clock→wire pump and
/// the (single, global) effect broker per sidebar mount. Call **once** from the
/// `App::run` body after services are registered — the reconcilers themselves
/// mount per-monitor via [`sidebar_top_slot`] / [`sidebar_bottom_slot`].
pub fn install() {
    // Clock state pump: project the live `clock::now()` into a GTK-free wire
    // `ClockState` and publish it on the watch channel the per-conn snapshot
    // tasks subscribe to. `clock::now()` replays its current value on subscribe,
    // so a plugin that dials in later still gets an initial snapshot.
    glib::MainContext::default().spawn_local(clock::now().for_each(|dt| {
        set_clock(to_clock_state(&dt));
        std::future::ready(())
    }));

    // Effect broker: one global subscription per sidebar mailbox so a bundled
    // effect is brokered exactly once even when the same tree is mirrored onto
    // several monitors' sidebars. Reconcilers (per monitor) render the tree but
    // never touch effects.
    glib::MainContext::default().spawn_local(top_render_signal().for_each(|opt| {
        broker_render_effects(opt.as_ref());
        std::future::ready(())
    }));
    glib::MainContext::default().spawn_local(bottom_render_signal().for_each(|opt| {
        broker_render_effects(opt.as_ref());
        std::future::ready(())
    }));
}

/// Publish the latest clock state to the per-conn snapshot tasks.
fn set_clock(cs: ClockState) {
    registry::with(|r| {
        r.get::<PluginHandles>()
            .expect("plugins::service() not registered")
            .clock_tx
            .send_replace(Some(cs));
    });
}

fn top_render_signal() -> impl Signal<Item = Option<SlotRender>> {
    registry::with(|r| {
        r.get::<PluginHandles>()
            .expect("plugins::service() not registered")
            .sidebar_top
            .signal_cloned()
    })
}

fn bottom_render_signal() -> impl Signal<Item = Option<SlotRender>> {
    registry::with(|r| {
        r.get::<PluginHandles>()
            .expect("plugins::service() not registered")
            .sidebar_bottom
            .signal_cloned()
    })
}

/// Broker every effect bundled on a render frame (v1: `OpenPage` only).
fn broker_render_effects(render: Option<&SlotRender>) {
    let Some(render) = render else {
        return;
    };
    for effect in &render.effects {
        broker_effect(&render.plugin_id, effect);
    }
}

/// Map one wire [`Effect`] onto a real host command. v1 handles [`Effect::OpenPage`]
/// only (→ the modal drawer); anything else is logged and skipped. Capability
/// gating + audit-log + the `RunCommand` round-trip are deferred (see the module
/// doc / PR body).
fn broker_effect(plugin_id: &str, effect: &Effect) {
    match effect {
        Effect::OpenPage(page) => {
            let target = map_page(*page);
            tracing::info!(plugin = %plugin_id, ?target, "plugin effect: OpenPage");
            crate::modal::open_on_focused(None, target);
        }
        other => {
            tracing::warn!(plugin = %plugin_id, ?other, "plugin effect unsupported in v1; skipped");
        }
    }
}

// ── GTK-side mount: reconciler-backed sidebar slots ──────────────────────────

/// A reconciler-backed [`Mount::SidebarTop`] slot. Built per monitor from
/// `overlays::sidebar::build_card` and appended above the built-in widgets.
#[must_use]
pub fn sidebar_top_slot() -> gtk::Widget {
    build_slot(top_render_signal())
}

/// A reconciler-backed [`Mount::SidebarBottom`] slot, appended below the
/// built-in sidebar widgets.
#[must_use]
pub fn sidebar_bottom_slot() -> gtk::Widget {
    build_slot(bottom_render_signal())
}

/// Build a `gtk::Box` driven by a [`Reconciler`] fed from `signal` (a mount's
/// render mailbox). The reconciler's `on_event` routes user interactions back
/// to whichever plugin currently owns the slot, via the outbound sender carried
/// on the latest [`SlotRender`].
fn build_slot(signal: impl Signal<Item = Option<SlotRender>> + 'static) -> gtk::Widget {
    let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
    container.add_css_class("ts-plugin-slot");

    // Outbound sender of the plugin currently mounted here. GTK-thread only, so
    // an `Rc<RefCell<…>>` is correct — this never crosses to tokio.
    let outbound: Rc<RefCell<Option<mpsc::UnboundedSender<HostMsg>>>> = Rc::new(RefCell::new(None));

    let event_outbound = outbound.clone();
    let reconciler = Reconciler::new(&container, move |id: NodeId, kind: UiEventKind| {
        if let Some(tx) = event_outbound.borrow().as_ref() {
            let _ = tx.send(HostMsg::Event {
                node: id,
                kind: to_wire_event(kind),
            });
        }
    });
    let reconciler = Rc::new(RefCell::new(reconciler));

    let rec = reconciler.clone();
    let out = outbound.clone();
    let handle = glib::MainContext::default().spawn_local(signal.for_each(move |opt| {
        if let Some(render) = opt {
            *out.borrow_mut() = Some(render.outbound.clone());
            rec.borrow_mut().render(&to_ui_node(&render.tree));
        } else {
            // Plugin disconnected / slot released: blank the slot and stop
            // routing events to the dead sender.
            *out.borrow_mut() = None;
            rec.borrow_mut().render(&empty_node());
        }
        std::future::ready(())
    }));

    // Best-effort teardown: abort the render subscription when the slot widget
    // is destroyed (a sidebar rebuild on hot-plug), so it stops rendering into a
    // detached container and drops its captured handles.
    container.connect_destroy(move |_| handle.abort());

    container.upcast()
}

// ── tokio-side: listener + per-connection tasks ──────────────────────────────

/// The socket path under `$XDG_RUNTIME_DIR`, or `None` if that env var is unset
/// (per the spec the host is same-user-only and refuses to fall back elsewhere).
fn socket_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")?;
    let mut path = PathBuf::from(base);
    path.push(SOCKET_DIR);
    path.push(SOCKET_FILE);
    Some(path)
}

/// Bind the host socket and accept plugin connections forever. Creates the
/// parent dir (`0700`), unlinks any stale socket before bind, and tightens the
/// socket to `0600`.
async fn listen(ctx: &ListenerCtx) -> std::io::Result<()> {
    let Some(path) = socket_path() else {
        tracing::warn!("XDG_RUNTIME_DIR unset; plugin host socket not created");
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
        // Same-user only. Best-effort — the runtime dir is already 0700.
        let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
    }
    // A stale socket left by a previous run makes `bind` fail with EADDRINUSE.
    match fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(&path)?;
    let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    tracing::info!(socket = %path.display(), "plugin host listening");

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let ctx = ctx.clone();
                tokio::spawn(async move {
                    handle_conn(stream, &ctx).await;
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "plugin host accept failed; stopping");
                return Err(e);
            }
        }
    }
}

/// Drive one plugin connection: handshake, then read frames until the peer
/// disconnects, feeding renders into the mount mailbox and pushing state
/// snapshots + events back out.
async fn handle_conn(stream: UnixStream, ctx: &ListenerCtx) {
    let (mut rd, wr) = stream.into_split();

    // Handshake: the first frame MUST be `Register`, and its proto must match
    // exactly — else drop the connection (schema skew fails loud).
    let first = match read_frame::<PluginMsg, _>(&mut rd).await {
        Ok(msg) => msg,
        Err(e) => {
            tracing::warn!(error = %e, "plugin handshake read failed; dropping");
            return;
        }
    };
    let manifest = match first {
        PluginMsg::Register { manifest } => manifest,
        other => {
            tracing::warn!(?other, "plugin's first frame was not Register; dropping");
            return;
        }
    };
    if let Err(e) = manifest.check_proto() {
        tracing::warn!(plugin = %manifest.id, error = %e, "plugin proto mismatch; dropping");
        return;
    }
    let plugin_id = manifest.id.clone();
    let mount = manifest.mount;
    tracing::info!(
        plugin = %plugin_id,
        ?mount,
        subscribes = ?manifest.subscribes,
        capabilities = ?manifest.capabilities,
        "plugin registered",
    );

    // Outbound writer: the single point that serializes host→plugin frames.
    let (out_tx, out_rx) = mpsc::unbounded_channel::<HostMsg>();
    let writer = tokio::spawn(writer_task(wr, out_rx));

    // Initial + on-change state snapshots (Clock only, if subscribed).
    let snapshot = manifest
        .subscribes
        .contains(&StateKey::Clock)
        .then(|| tokio::spawn(snapshot_task(ctx.clock_rx.clone(), out_tx.clone())));

    // Reader loop: dispatch inbound frames until the peer disconnects.
    loop {
        match read_frame::<PluginMsg, _>(&mut rd).await {
            Ok(PluginMsg::Render { tree, effects }) => {
                let render = SlotRender {
                    plugin_id: plugin_id.clone(),
                    tree,
                    effects,
                    outbound: out_tx.clone(),
                };
                // Latest-wins: `set` overwrites any superseded frame in place.
                match mount {
                    Mount::SidebarTop => ctx.sidebar_top.set(Some(render)),
                    Mount::SidebarBottom => ctx.sidebar_bottom.set(Some(render)),
                    Mount::BarLeft | Mount::BarCenter | Mount::BarRight => {
                        tracing::debug!(plugin = %plugin_id, ?mount, "bar mount unsupported in v1; render dropped");
                    }
                }
            }
            Ok(PluginMsg::Register { .. }) => {
                tracing::warn!(plugin = %plugin_id, "duplicate Register ignored");
            }
            Ok(PluginMsg::Log { level, msg }) => log_plugin(&plugin_id, level, &msg),
            Ok(PluginMsg::Pong { seq }) => tracing::trace!(plugin = %plugin_id, seq, "plugin pong"),
            Err(e) => {
                tracing::info!(plugin = %plugin_id, reason = %e, "plugin disconnected");
                break;
            }
        }
    }

    // Teardown: release the mount slot if this plugin still owns it (blanks the
    // reconciler on the GTK side), then stop the outbound + snapshot tasks.
    clear_slot_if_owned(&ctx.sidebar_top, &plugin_id);
    clear_slot_if_owned(&ctx.sidebar_bottom, &plugin_id);
    if let Some(snapshot) = snapshot {
        snapshot.abort();
    }
    writer.abort();
}

/// Serialize host→plugin frames pulled off the outbound channel until the
/// channel closes or a write fails.
async fn writer_task(mut wr: OwnedWriteHalf, mut rx: mpsc::UnboundedReceiver<HostMsg>) {
    while let Some(msg) = rx.recv().await {
        if let Err(e) = write_frame(&mut wr, &msg).await {
            tracing::debug!(error = %e, "plugin outbound write failed; closing writer");
            break;
        }
    }
}

/// Push the full subscribed state subset (v1: `clock`) on the initial subscribe
/// and on every change, coalescing bursts latest-wins via `borrow_and_update`.
async fn snapshot_task(
    mut clock_rx: watch::Receiver<Option<ClockState>>,
    out: mpsc::UnboundedSender<HostMsg>,
) {
    // Initial snapshot (the watch replays its current value).
    let initial = clock_rx.borrow_and_update().clone();
    if out
        .send(HostMsg::StateSnapshot {
            snapshot: StateSnapshot { clock: initial },
        })
        .is_err()
    {
        return;
    }
    while clock_rx.changed().await.is_ok() {
        let clock = clock_rx.borrow_and_update().clone();
        if out
            .send(HostMsg::StateSnapshot {
                snapshot: StateSnapshot { clock },
            })
            .is_err()
        {
            break;
        }
    }
}

/// Clear a mount slot iff the disconnecting plugin still owns it (a newer plugin
/// may already have claimed the slot).
fn clear_slot_if_owned(slot: &Mutable<Option<SlotRender>>, plugin_id: &str) {
    let owned = {
        slot.lock_ref()
            .as_ref()
            .is_some_and(|r| r.plugin_id == plugin_id)
    };
    if owned {
        slot.set(None);
    }
}

/// Surface a plugin's `Log` frame at the matching host `tracing` level.
fn log_plugin(plugin_id: &str, level: LogLevel, msg: &str) {
    match level {
        LogLevel::Error => tracing::error!(plugin = %plugin_id, "{msg}"),
        LogLevel::Warn => tracing::warn!(plugin = %plugin_id, "{msg}"),
        LogLevel::Info => tracing::info!(plugin = %plugin_id, "{msg}"),
        LogLevel::Debug => tracing::debug!(plugin = %plugin_id, "{msg}"),
        LogLevel::Trace => tracing::trace!(plugin = %plugin_id, "{msg}"),
    }
}

// ── wire ⇄ host mappings (mechanical, but exhaustive) ────────────────────────

/// Project `clock::now()`'s `DateTime<Local>` into the GTK-/chrono-free wire
/// [`ClockState`].
fn to_clock_state(dt: &DateTime<Local>) -> ClockState {
    ClockState {
        iso: dt.to_rfc3339(),
        unix: dt.timestamp(),
    }
}

/// Map a wire [`wire::Node`] onto the reconciler's `hytte_ui::Node`. The two
/// mirror each other field-for-field (#266), so this is a 1:1 recursion — but it
/// is written exhaustively so adding a node variant to either side is a compile
/// error here.
fn to_ui_node(node: &wire::Node) -> UiNode {
    match node {
        wire::Node::Box {
            id,
            dir,
            spacing,
            scroll,
            classes,
            children,
        } => UiNode::Box {
            id: id.clone(),
            dir: to_ui_dir(*dir),
            spacing: *spacing,
            scroll: *scroll,
            classes: classes.clone(),
            children: children.iter().map(to_ui_node).collect(),
        },
        wire::Node::Label { id, text, classes } => UiNode::Label {
            id: id.clone(),
            text: text.clone(),
            classes: classes.clone(),
        },
        wire::Node::Icon { id, name, classes } => UiNode::Icon {
            id: id.clone(),
            name: name.clone(),
            classes: classes.clone(),
        },
        wire::Node::Button { id, classes, child } => UiNode::Button {
            id: id.clone(),
            classes: classes.clone(),
            child: Box::new(to_ui_node(child)),
        },
        wire::Node::Progress {
            id,
            fraction,
            classes,
        } => UiNode::Progress {
            id: id.clone(),
            fraction: *fraction,
            classes: classes.clone(),
        },
        wire::Node::Revealer { id, open, child } => UiNode::Revealer {
            id: id.clone(),
            open: *open,
            child: Box::new(to_ui_node(child)),
        },
        wire::Node::Separator { classes } => UiNode::Separator {
            classes: classes.clone(),
        },
    }
}

fn to_ui_dir(dir: wire::Dir) -> UiDir {
    match dir {
        wire::Dir::Horizontal => UiDir::Horizontal,
        wire::Dir::Vertical => UiDir::Vertical,
    }
}

/// Map a reconciler event back onto its wire form for the outbound `Event`
/// frame. Exhaustive over the v1 `EventKind` set (Click + Scroll).
fn to_wire_event(kind: UiEventKind) -> wire::EventKind {
    match kind {
        UiEventKind::Click => wire::EventKind::Click,
        UiEventKind::Scroll { dx, dy } => wire::EventKind::Scroll { dx, dy },
    }
}

/// Map a wire [`Page`] onto the host's `modal::Page`. The two enums mirror each
/// other; written exhaustively so a page added to either side breaks the build
/// here rather than silently mis-routing.
fn map_page(page: Page) -> crate::modal::Page {
    use crate::modal::Page as M;
    match page {
        Page::Media => M::Media,
        Page::Network => M::Network,
        Page::Vpn => M::Vpn,
        Page::Connections => M::Connections,
        Page::Bluetooth => M::Bluetooth,
        Page::Stats => M::Stats,
        Page::Audio => M::Audio,
        Page::Power => M::Power,
        Page::PowerMenu => M::PowerMenu,
        Page::Notifications => M::Notifications,
        Page::Appearance => M::Appearance,
        Page::Displays => M::Displays,
        Page::Clipboard => M::Clipboard,
        Page::Calendar => M::Calendar,
        Page::Settings => M::Settings,
    }
}

/// An empty root node used to blank a slot when its plugin disconnects.
fn empty_node() -> UiNode {
    UiNode::Box {
        id: None,
        dir: UiDir::Vertical,
        spacing: 0,
        scroll: false,
        classes: Vec::new(),
        children: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Effect, HostMsg, Mount, Mutable, Page, SlotRender, StateKey, UiDir, UiEventKind, UiNode,
        empty_node, map_page, mpsc, to_ui_node, to_wire_event, wire,
    };

    /// The `wire`→`hytte_ui` mapping is exhaustive over every node variant
    /// (incl. `Box { scroll }` and nesting) and produces a field-for-field
    /// mirror.
    #[test]
    fn wire_node_maps_to_ui_node_exhaustively() {
        let tree = wire::Node::Box {
            id: Some("root".into()),
            dir: wire::Dir::Vertical,
            spacing: 4,
            scroll: true,
            classes: vec!["ts-a".into()],
            children: vec![
                wire::Node::Label {
                    id: None,
                    text: "hi".into(),
                    classes: vec!["ts-l".into()],
                },
                wire::Node::Icon {
                    id: Some("i".into()),
                    name: "battery-symbolic".into(),
                    classes: vec![],
                },
                wire::Node::Button {
                    id: "b".into(),
                    classes: vec!["ts-btn".into()],
                    child: Box::new(wire::Node::Label {
                        id: None,
                        text: "go".into(),
                        classes: vec![],
                    }),
                },
                wire::Node::Progress {
                    id: None,
                    fraction: 0.5,
                    classes: vec![],
                },
                wire::Node::Revealer {
                    id: Some("r".into()),
                    open: true,
                    child: Box::new(wire::Node::Separator {
                        classes: vec!["ts-sep".into()],
                    }),
                },
                wire::Node::Box {
                    id: None,
                    dir: wire::Dir::Horizontal,
                    spacing: 0,
                    scroll: false,
                    classes: vec![],
                    children: vec![],
                },
            ],
        };
        let expected = UiNode::Box {
            id: Some("root".into()),
            dir: UiDir::Vertical,
            spacing: 4,
            scroll: true,
            classes: vec!["ts-a".into()],
            children: vec![
                UiNode::Label {
                    id: None,
                    text: "hi".into(),
                    classes: vec!["ts-l".into()],
                },
                UiNode::Icon {
                    id: Some("i".into()),
                    name: "battery-symbolic".into(),
                    classes: vec![],
                },
                UiNode::Button {
                    id: "b".into(),
                    classes: vec!["ts-btn".into()],
                    child: Box::new(UiNode::Label {
                        id: None,
                        text: "go".into(),
                        classes: vec![],
                    }),
                },
                UiNode::Progress {
                    id: None,
                    fraction: 0.5,
                    classes: vec![],
                },
                UiNode::Revealer {
                    id: Some("r".into()),
                    open: true,
                    child: Box::new(UiNode::Separator {
                        classes: vec!["ts-sep".into()],
                    }),
                },
                UiNode::Box {
                    id: None,
                    dir: UiDir::Horizontal,
                    spacing: 0,
                    scroll: false,
                    classes: vec![],
                    children: vec![],
                },
            ],
        };
        assert_eq!(to_ui_node(&tree), expected);
    }

    #[test]
    fn empty_node_is_an_empty_box() {
        assert_eq!(
            empty_node(),
            UiNode::Box {
                id: None,
                dir: UiDir::Vertical,
                spacing: 0,
                scroll: false,
                classes: vec![],
                children: vec![],
            }
        );
    }

    #[test]
    fn ui_event_maps_to_wire_event() {
        assert_eq!(to_wire_event(UiEventKind::Click), wire::EventKind::Click);
        assert_eq!(
            to_wire_event(UiEventKind::Scroll { dx: 1.5, dy: -2.0 }),
            wire::EventKind::Scroll { dx: 1.5, dy: -2.0 }
        );
    }

    /// Every wire `Page` maps to the identically-named `modal::Page`.
    #[test]
    fn wire_page_maps_to_modal_page() {
        use crate::modal::Page as M;
        let cases = [
            (Page::Media, M::Media),
            (Page::Network, M::Network),
            (Page::Vpn, M::Vpn),
            (Page::Connections, M::Connections),
            (Page::Bluetooth, M::Bluetooth),
            (Page::Stats, M::Stats),
            (Page::Audio, M::Audio),
            (Page::Power, M::Power),
            (Page::PowerMenu, M::PowerMenu),
            (Page::Notifications, M::Notifications),
            (Page::Appearance, M::Appearance),
            (Page::Displays, M::Displays),
            (Page::Clipboard, M::Clipboard),
            (Page::Calendar, M::Calendar),
            (Page::Settings, M::Settings),
        ];
        for (wire_page, modal_page) in cases {
            assert_eq!(map_page(wire_page), modal_page);
        }
    }

    /// The render mailbox coalesces latest-wins: rapid frames overwrite in place,
    /// and a slow consumer reading once observes only the newest tree.
    #[test]
    fn render_mailbox_coalesces_latest_wins() {
        let (tx, _rx) = mpsc::unbounded_channel::<HostMsg>();
        let frame = |text: &str| SlotRender {
            plugin_id: "p".into(),
            tree: wire::Node::Label {
                id: None,
                text: text.to_owned(),
                classes: vec![],
            },
            effects: vec![],
            outbound: tx.clone(),
        };
        let mailbox: Mutable<Option<SlotRender>> = Mutable::new(None);
        mailbox.set(Some(frame("first")));
        mailbox.set(Some(frame("second")));
        mailbox.set(Some(frame("third")));

        let guard = mailbox.lock_ref();
        let latest = guard.as_ref().expect("mailbox holds the latest frame");
        assert_eq!(latest.plugin_id, "p");
        match &latest.tree {
            wire::Node::Label { text, .. } => assert_eq!(text, "third"),
            other => panic!("expected a Label, got {other:?}"),
        }
    }

    /// A bar mount is a real wire variant the reader must handle; assert the two
    /// sidebar mounts we support are distinct from the deferred bar mounts.
    #[test]
    fn sidebar_and_bar_mounts_are_distinct() {
        assert_ne!(Mount::SidebarTop, Mount::BarLeft);
        assert_ne!(Mount::SidebarBottom, Mount::BarCenter);
        // Effect + StateKey are exercised elsewhere; touch them here so the test
        // module's imports stay honest if the broker/pump code is refactored.
        assert_eq!(StateKey::Clock, StateKey::Clock);
        assert!(matches!(Effect::OpenPage(Page::Media), Effect::OpenPage(_)));
    }
}
