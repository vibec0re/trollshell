//! Native `PipeWire` audio backend (work in progress).
//!
//! This module is being built phase by phase to replace the `pactl`/`wpctl`
//! shell-out in [`super::pipewire`]. Until Phase 6 lands, it is NOT wired
//! into the service registry — callers still go through `services::pipewire`
//! which keeps shelling out. Only `cargo check` exercises this code today.
//!
//! # Threading
//!
//! `libpipewire` is not `Send` / `Sync`. The mainloop, context, core, and
//! registry are all `Rc`-typed in the Rust bindings and must live on one
//! thread. We follow the canonical pipewire-rs pattern: spawn a dedicated
//! `std::thread`, build the mainloop there, run it indefinitely. Commands
//! from the tokio side cross the thread boundary via
//! [`pipewire::channel`], which the mainloop polls as a loop source.
//! Outbound state (sink/source/stream snapshots) lands in `Mutable`s from
//! `futures_signals`, which are `Send + Sync`, so subscribers on the GTK
//! main thread read them without further glue.
//!
//! # Phase 1 (done)
//!
//! Scaffold: mainloop owner thread, registry walker, Node classification
//! by `media.class`, monitor-source filtering, Metadata + Link logging.
//!
//! # Phase 2
//!
//! For each classified Audio Node, `registry.bind` a `Node` proxy, attach
//! a `.param` listener, and `subscribe_params(&[ParamType::Props])`.
//! Decode the `spa_pod` payload: `SPA_PROP_channelVolumes` (array of f32
//! linear gains per channel) and `SPA_PROP_mute` (bool). Walk the cache
//! after every change and `Mutable::set` fresh `Vec<Sink>`, `Vec<Source>`,
//! `Vec<PlaybackStream>`, `Vec<RecordStream>`, and `Volume` snapshots into
//! the existing `super::pipewire::PipewireHandles` so cutover in Phase 6
//! is a one-line swap.
//!
//! # Phase 3
//!
//! Mutation. A [`pipewire::channel`] bridges tokio-side callers into the
//! pw-loop thread. The eight `set_*` / `toggle_*` functions enqueue a
//! [`Command`], the receiver attached to the loop resolves the target node
//! by name (sinks/sources) or by id (streams), builds a `SPA_TYPE_OBJECT_\
//! Props` pod via [`libspa::pod`], and calls `node.set_param(Props, 0, pod)`.
//! For volume, the builder preserves the live channel count from the cache
//! so a stereo sink stays stereo after `set_sink_volume`.
//!
//! # Phase 4
//!
//! Default sink/source resolution via the `default` Metadata global. We
//! bind it, listen for property events with keys `default.audio.sink` and
//! `default.audio.source` (values are JSON: `{"name":"<node.name>"}`), and
//! cache the resolved names. After every metadata change `emit_snapshots`
//! flags the matching Sink/Source with `is_default = true` and the
//! `Volume` signal at the bar reflects the current default sink's level.
//! Writes go the other way via `Metadata::set_property`.
//!
//! # Phase 5 (this commit)
//!
//! Stream→sink routing via `PipeWire` Link globals. Each Link's props dict
//! carries `link.output.node` and `link.input.node` (pipewire node ids).
//! We index links by their output node id; on snapshot emission, every
//! `PlaybackStream` reads its `sink_id` from the link whose output is the
//! stream itself. `RecordStreams` mirror: `source_id` comes from the link's
//! `output` node (the source) when the stream is the `input`. With this
//! the audio modal can group streams under their target sink.

use hytte_reactive::{registry, Service};
use pipewire as pw;
use pw::types::ObjectType;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::OnceLock;
use std::thread;

use super::pipewire::{
    PipewireHandles, PlaybackStream, RecordStream, Sink, Source, Volume,
};

/// Coarse classification of a `PipeWire` Node by its `media.class` property.
///
/// `PipeWire` nodes can be many things (cameras, MIDI, ALSA cards, virtual
/// devices, application streams). For volume-control purposes only four
/// kinds matter; everything else is filtered out at registry-walk time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AudioRole {
    /// `media.class = Audio/Sink` — output device (speakers, headphones).
    Sink,
    /// `media.class = Audio/Source` — input device (microphone, line-in).
    /// Excludes `.monitor` sources, which are filtered by node.name suffix.
    Source,
    /// `media.class = Stream/Output/Audio` — playback stream from an app
    /// (Firefox tab, Spotify, etc.) producing audio that gets linked to
    /// an Audio/Sink.
    OutputStream,
    /// `media.class = Stream/Input/Audio` — recording stream from an app
    /// (browser mic, conference call) consuming from an Audio/Source.
    InputStream,
}

impl AudioRole {
    fn from_media_class(class: &str) -> Option<Self> {
        match class {
            "Audio/Sink" => Some(Self::Sink),
            "Audio/Source" => Some(Self::Source),
            "Stream/Output/Audio" => Some(Self::OutputStream),
            "Stream/Input/Audio" => Some(Self::InputStream),
            _ => None,
        }
    }
}

/// Cached identity + volume state of a single audio Node.
#[derive(Clone, Debug)]
struct NodeEntry {
    role: AudioRole,
    /// `node.name` — the canonical id pactl uses and the one that survives
    /// across reconnects. Used to mark `is_default` against the Metadata
    /// `default.audio.sink` value, and as the argument to `set_default_sink`.
    name: String,
    /// `node.description` — human-readable; falls back to `name`.
    description: String,
    /// For streams: best-effort app name. `None` for Audio/Sink, Audio/Source.
    app_name: Option<String>,
    /// Per-channel linear gain, populated from `SPA_PROP_channelVolumes`.
    /// Empty until the first Props event arrives. `PipeWire`'s channelVolumes
    /// is already in linear-gain space (1.0 = 100%), matching the `Sink`
    /// public API field — no cube transform needed.
    channel_volumes: Vec<f32>,
    /// `SPA_PROP_mute`. Defaults to `false` until the first Props event.
    mute: bool,
}

/// Per-node proxy + listener pair. Both must stay alive until the node
/// global is removed, otherwise the C-side closure is dropped and the
/// param events stop arriving.
struct NodeProxy {
    #[allow(dead_code)] // kept alive solely for its Drop side-effects
    proxy: pw::node::Node,
    #[allow(dead_code)]
    listener: pw::node::NodeListener,
}

/// Proxy + listener for the single `default` Metadata global. Pipewire
/// emits multiple Metadata instances (e.g. per-device route metadata); we
/// bind only the one named `default`. Like `NodeProxy`, both must stay
/// alive together — dropping either ends property events.
struct MetadataProxy {
    #[allow(dead_code)]
    proxy: pw::metadata::Metadata,
    #[allow(dead_code)]
    listener: pw::metadata::MetadataListener,
}

/// One edge of the `PipeWire` graph — used to resolve stream→sink (or
/// stream→source) routing without binding the Link proxy. The values are
/// the node ids on each side; the Link's own id is the [`AudioState::links`]
/// map key.
#[derive(Clone, Copy, Debug)]
struct LinkEdge {
    output_node: u32,
    input_node: u32,
}

/// State owned by the pipewire-loop thread. Wrapped in `Rc<RefCell<_>>` so
/// registry-event callbacks (each its own `FnMut`-bound closure) share access.
struct AudioState {
    /// Node identity + volume cache, keyed by `PipeWire` global id.
    nodes: HashMap<u32, NodeEntry>,
    /// Live proxies for every bound Node. Dropping the entry destroys the
    /// proxy and detaches its listener, ending param events.
    proxies: HashMap<u32, NodeProxy>,
    /// Live proxy for the `default` Metadata global (one per session).
    /// Populated lazily when the registry walker first sees a Metadata
    /// global with `metadata.name = "default"`.
    metadata_default: Option<MetadataProxy>,
    /// `default.audio.sink` value — the `node.name` of the current default
    /// sink. Compared against each Sink's name in `emit_snapshots` to
    /// flip `is_default`. `None` until the first property event fires.
    default_sink_name: Option<String>,
    /// Same as `default_sink_name` for the default source.
    default_source_name: Option<String>,
    /// Graph-edge cache, keyed by Link global id. Built from registry
    /// add events (the props dict carries `link.{output,input}.node`).
    /// Stream→sink routing is derived from this in `emit_snapshots` by
    /// finding the link whose `output_node` matches the stream id.
    links: HashMap<u32, LinkEdge>,
    /// Output Mutables — fresh snapshots are pushed here after every state
    /// change. Cloning a `Mutable` clones the `Arc` inside, so this struct
    /// shares ownership with whatever the `Service::start` caller holds.
    handles: PipewireHandles,
    /// Last snapshot pushed to each Mutable, used to skip redundant
    /// `set()` calls that would tear down subscribers' diff state for a
    /// no-op. The Mutables already dedup by `PartialEq`, but doing the
    /// comparison here saves the Vec clone on a hot path.
    last_sink_volume: Volume,
    last_sinks: Vec<Sink>,
    last_sources: Vec<Source>,
    last_streams: Vec<PlaybackStream>,
    last_record_streams: Vec<RecordStream>,
}

impl AudioState {
    fn new(handles: PipewireHandles) -> Self {
        Self {
            nodes: HashMap::new(),
            proxies: HashMap::new(),
            metadata_default: None,
            default_sink_name: None,
            default_source_name: None,
            links: HashMap::new(),
            handles,
            last_sink_volume: Volume::default(),
            last_sinks: Vec::new(),
            last_sources: Vec::new(),
            last_streams: Vec::new(),
            last_record_streams: Vec::new(),
        }
    }
}

/// Cross-thread command sent from tokio callers (UI gestures, keybindings)
/// to the pipewire-loop thread. Decoded inside the loop's attached
/// [`pw::channel::Receiver`] and dispatched to the matching node's
/// `set_param`.
///
/// All variants take owned strings rather than borrowing because the loop
/// receives messages asynchronously and may process them after the caller's
/// stack has unwound.
#[derive(Clone, Debug)]
// `Set*` prefix on every variant is intentional — this is a command enum.
#[allow(clippy::enum_variant_names)]
enum Command {
    /// Set channelVolumes on a sink, identified by `node.name`. The new
    /// linear gain is replicated across every channel currently in the
    /// cache so a stereo sink stays stereo. If the cache is empty (no
    /// Props event received yet) the command is dropped — we'd otherwise
    /// publish a mono channelVolumes array and clobber the sink's layout.
    SetSinkVolume { name: String, linear: f64 },
    /// Set channelVolumes on a source (microphone, line-in).
    SetSourceVolume { name: String, linear: f64 },
    /// Set channelVolumes on a playback stream, identified by pipewire
    /// global id (streams have no stable `node.name`).
    SetStreamVolume { id: u32, linear: f64 },
    /// Set `SPA_PROP_mute` on a sink.
    SetSinkMute { name: String, mute: bool },
    SetSourceMute { name: String, mute: bool },
    SetStreamMute { id: u32, mute: bool },
    /// Write `default.audio.sink` on the `default` Metadata. Phase 4 wires
    /// this — for Phase 3 the dispatcher logs and drops the command.
    SetDefaultSink { name: String },
    SetDefaultSource { name: String },
}

/// Sender shared across all callers. Populated by [`spawn_mainloop`] before
/// the loop runs; mutation fns read it with `.get()`. A `OnceLock` keeps
/// this thread-safe without requiring a `Mutex`, and an inert clone after
/// `spawn_mainloop` returns is enough to send commands from any thread.
static COMMAND_TX: OnceLock<pw::channel::Sender<Command>> = OnceLock::new();

/// Drives the dedicated pipewire-loop thread. Re-exported from
/// [`super::pipewire`] as `pipewire::PipewireService` so callers keep
/// using the historical `services::pipewire::service()` path.
pub struct PipewireService;

impl Service for PipewireService {
    type Handles = PipewireHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = PipewireHandles::default();
        let handles_for_thread = clone_handles(&handles);
        spawn_mainloop(handles_for_thread);
        handles
    }
}

/// Clone a `PipewireHandles` for cross-thread sharing. Each `Mutable` is
/// internally an `Arc` so clones see the same backing storage.
fn clone_handles(h: &PipewireHandles) -> PipewireHandles {
    PipewireHandles {
        sink: h.sink.clone(),
        sinks: h.sinks.clone(),
        sources: h.sources.clone(),
        streams: h.streams.clone(),
        record_streams: h.record_streams.clone(),
    }
}

/// Spawn the pipewire mainloop thread. Returns immediately; the thread
/// runs for the lifetime of the process. Errors during init (e.g. no
/// `/run/user/$UID/pipewire-0` socket) are logged and the thread retries
/// after a short backoff so a daemon restart heals automatically.
///
/// Creates the command channel up front and installs the [`Sender`] into
/// [`COMMAND_TX`] before the thread starts running the loop. That way any
/// `set_*` call from the tokio side that lands before the loop has fully
/// connected to pipewire goes through the channel and is buffered until
/// the receiver attaches — never silently dropped.
///
/// [`Sender`]: pw::channel::Sender
fn spawn_mainloop(handles: PipewireHandles) {
    let (tx, rx) = pw::channel::channel::<Command>();
    if COMMAND_TX.set(tx).is_err() {
        // Programmer error: start() called twice. Don't overwrite the live
        // sender — the second mainloop wouldn't share the first's proxy
        // map and writes would silently no-op.
        tracing::warn!("audio_native: spawn_mainloop called twice; ignoring second start");
        return;
    }
    // The `pw::channel::Receiver` is not `Send`, so we hand it through a
    // local `Cell<Option<_>>` style swap: the closure below moves it into
    // the thread, where it gets `take()`n on first iteration of the retry
    // loop and re-attached to each fresh mainloop. Concretely, since
    // mainloop crashes (no daemon, dbus glitch) restart the whole loop,
    // the receiver must outlive `run_once`. `pipewire::channel::Receiver`
    // detaches cleanly when `AttachedReceiver` is dropped, so the next
    // run_once just attaches again.
    let mut rx = Some(rx);
    thread::Builder::new()
        .name("hytte-audio-pw".into())
        .spawn(move || {
            pw::init();
            loop {
                let handles = clone_handles(&handles);
                let receiver = rx.take().expect("audio_native: receiver consumed twice");
                let (returned_rx, res) = run_once(handles, receiver);
                rx = Some(returned_rx);
                if let Err(e) = res {
                    tracing::warn!(error = ?e, "audio_native: mainloop exited, retrying in 1s");
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
            }
        })
        .expect("spawn audio_native mainloop thread");
}

/// One mainloop session. Returns the receiver (so `spawn_mainloop` can
/// re-attach it on the next session) along with the run result.
// One cohesive PipeWire registry + listener wiring block; splitting it would
// scatter shared closure state across helpers for no real readability gain.
#[allow(clippy::too_many_lines)]
fn run_once(
    handles: PipewireHandles,
    rx: pw::channel::Receiver<Command>,
) -> (pw::channel::Receiver<Command>, Result<(), pw::Error>) {
    let mainloop = match pw::main_loop::MainLoopRc::new(None) {
        Ok(m) => m,
        Err(e) => return (rx, Err(e)),
    };
    let context = match pw::context::ContextRc::new(&mainloop, None) {
        Ok(c) => c,
        Err(e) => return (rx, Err(e)),
    };
    let core = match context.connect_rc(None) {
        Ok(c) => c,
        Err(e) => return (rx, Err(e)),
    };
    let registry = match core.get_registry_rc() {
        Ok(r) => r,
        Err(e) => return (rx, Err(e)),
    };

    let state = Rc::new(RefCell::new(AudioState::new(handles)));

    // Attach the command channel. The returned `AttachedReceiver` must
    // outlive the loop; deattached after `mainloop.run()` returns so the
    // bare `Receiver` can be re-attached on the next session.
    let state_for_cmds = Rc::clone(&state);
    let attached = rx.attach(mainloop.loop_(), move |cmd| {
        handle_command(cmd, &state_for_cmds);
    });

    // Core error → quit the mainloop so run_once returns cleanly and the
    // outer loop reconnects. Without this, daemon crashes leave the
    // mainloop blocked forever in the C-side poll.
    let mainloop_weak = mainloop.downgrade();
    let _core_listener = core
        .add_listener_local()
        .error(move |id, _seq, _res, message| {
            if id == 0 {
                tracing::warn!(message = %message, "audio_native: core error, quitting");
                if let Some(m) = mainloop_weak.upgrade() {
                    m.quit();
                }
            }
        })
        .register();

    // Registry add/remove callbacks. The returned listener handle keeps the
    // C-side callback alive — it must outlive the mainloop, hence the bind
    // to `_listener`.
    let registry_for_bind = registry.clone();
    let state_add = Rc::clone(&state);
    let state_remove = Rc::clone(&state);
    let _listener = registry
        .add_listener_local()
        .global(move |obj| {
            // Globals without a props dict can't carry media.class — skip.
            let Some(props) = obj.props.as_ref() else {
                return;
            };
            match obj.type_ {
                ObjectType::Node => {
                    let Some(class) = props.get("media.class") else {
                        return;
                    };
                    let Some(role) = AudioRole::from_media_class(class) else {
                        return;
                    };
                    let name = props.get("node.name").unwrap_or("").to_string();
                    // Drop monitor sources (loopback from sinks); the
                    // bar/audio modal hides them just like pactl does.
                    if role == AudioRole::Source && name.ends_with(".monitor") {
                        return;
                    }
                    let description = props
                        .get("node.description").map_or_else(|| name.clone(), str::to_owned);
                    let app_name = matches!(
                        role,
                        AudioRole::OutputStream | AudioRole::InputStream
                    )
                    .then(|| pick_app_name(props));

                    tracing::debug!(
                        id = obj.id,
                        role = ?role,
                        name = %name,
                        description = %description,
                        app = ?app_name,
                        "audio_native: + node",
                    );

                    state_add.borrow_mut().nodes.insert(
                        obj.id,
                        NodeEntry {
                            role,
                            name,
                            description,
                            app_name,
                            channel_volumes: Vec::new(),
                            mute: false,
                        },
                    );

                    // Bind a proxy so we can receive param events for
                    // volume + mute. Errors here usually mean the global
                    // was destroyed mid-bind; log and move on.
                    match bind_node_for_params(
                        &registry_for_bind,
                        obj.id,
                        Rc::clone(&state_add),
                    ) {
                        Ok(proxy) => {
                            state_add.borrow_mut().proxies.insert(obj.id, proxy);
                        }
                        Err(e) => {
                            tracing::warn!(
                                id = obj.id,
                                error = ?e,
                                "audio_native: bind node failed",
                            );
                        }
                    }

                    // Emit a snapshot so subscribers see the new node
                    // even before the first param event lands (with
                    // placeholder volume = 0). The next param event
                    // will overwrite with real values.
                    emit_snapshots(&mut state_add.borrow_mut());
                }
                ObjectType::Metadata => {
                    let name = props.get("metadata.name").unwrap_or("");
                    tracing::debug!(id = obj.id, name = %name, "audio_native: + metadata");
                    // Bind only the `default` metadata; per-device route
                    // metadata exists but is irrelevant for volume.
                    if name == "default" {
                        match bind_default_metadata(
                            &registry_for_bind,
                            obj.id,
                            Rc::clone(&state_add),
                        ) {
                            Ok(proxy) => {
                                state_add.borrow_mut().metadata_default = Some(proxy);
                            }
                            Err(e) => {
                                tracing::warn!(
                                    id = obj.id,
                                    error = ?e,
                                    "audio_native: bind default metadata failed",
                                );
                            }
                        }
                    }
                }
                ObjectType::Link => {
                    // Link globals carry endpoint node ids in their props
                    // dict (no need to bind a proxy). When either field
                    // is missing or unparseable the link is incomplete
                    // — pipewire occasionally surfaces those during
                    // graph reshuffles. Skip rather than caching half
                    // an edge.
                    let Some(out) = props
                        .get("link.output.node")
                        .and_then(|s| s.parse::<u32>().ok())
                    else {
                        return;
                    };
                    let Some(input) = props
                        .get("link.input.node")
                        .and_then(|s| s.parse::<u32>().ok())
                    else {
                        return;
                    };
                    tracing::trace!(
                        id = obj.id,
                        out,
                        input,
                        "audio_native: + link",
                    );
                    let mut s = state_add.borrow_mut();
                    s.links.insert(
                        obj.id,
                        LinkEdge {
                            output_node: out,
                            input_node: input,
                        },
                    );
                    emit_snapshots(&mut s);
                }
                _ => {}
            }
        })
        .global_remove(move |id| {
            let mut s = state_remove.borrow_mut();
            // Drop the proxy first so the listener detaches before we
            // touch any other fields it might reference.
            s.proxies.remove(&id);
            if let Some(removed) = s.nodes.remove(&id) {
                tracing::debug!(
                    id,
                    role = ?removed.role,
                    name = %removed.name,
                    "audio_native: - node",
                );
                emit_snapshots(&mut s);
            }
            // Removed Link → drop the cached edge. Affects stream
            // routing in the next snapshot.
            if s.links.remove(&id).is_some() {
                tracing::trace!(id, "audio_native: - link");
                emit_snapshots(&mut s);
            }
            // If the default Metadata global went away (pipewire-pulse or
            // wireplumber restart), drop our cached proxy so the next
            // re-add rebinds cleanly. We don't know the id of the metadata
            // global without tracking it; compare by checking whether
            // the proxy's upcast id matches. Cheaper to clear on any
            // removal that hits a Metadata-shaped slot.
            if let Some(meta) = s.metadata_default.as_ref()
                && pw::proxy::ProxyT::upcast_ref(&meta.proxy).id() == id
            {
                tracing::debug!(id, "audio_native: - default metadata");
                s.metadata_default = None;
                s.default_sink_name = None;
                s.default_source_name = None;
                emit_snapshots(&mut s);
            }
        })
        .register();

    mainloop.run();
    let rx = attached.deattach();
    (rx, Ok(()))
}

/// Bind a Node proxy and start receiving `Props` param events. The Node and
/// its `NodeListener` are returned together so the caller can keep them in
/// the proxy map; dropping either ends event delivery.
fn bind_node_for_params(
    registry: &pw::registry::RegistryRc,
    id: u32,
    state: Rc<RefCell<AudioState>>,
) -> Result<NodeProxy, pw::Error> {
    let node: pw::node::Node = registry.bind(&pw::registry::GlobalObject {
        id,
        permissions: pw::permissions::PermissionFlags::empty(),
        type_: ObjectType::Node,
        version: 3,
        props: None::<&pw::spa::utils::dict::DictRef>,
    })?;

    let listener = node
        .add_listener_local()
        .param(move |_seq, param_type, _index, _next, pod| {
            if param_type != pw::spa::param::ParamType::Props {
                return;
            }
            let Some(pod) = pod else { return };
            let bytes = pod.as_bytes();
            let Some((channel_volumes, mute)) = decode_props(bytes) else {
                return;
            };
            let mut s = state.borrow_mut();
            if let Some(entry) = s.nodes.get_mut(&id) {
                let mut changed = false;
                if let Some(cv) = channel_volumes
                    && cv != entry.channel_volumes {
                        entry.channel_volumes = cv;
                        changed = true;
                    }
                if let Some(m) = mute
                    && m != entry.mute {
                        entry.mute = m;
                        changed = true;
                    }
                if changed {
                    emit_snapshots(&mut s);
                }
            }
        })
        .register();

    node.subscribe_params(&[pw::spa::param::ParamType::Props]);

    Ok(NodeProxy {
        proxy: node,
        listener,
    })
}

/// Bind the `default` Metadata global and start listening for property
/// changes on `default.audio.sink` and `default.audio.source`. Other keys
/// (file-chooser default folder, screen-share preferences, etc.) also
/// live on this object; we filter inside the callback to leave those
/// untouched.
fn bind_default_metadata(
    registry: &pw::registry::RegistryRc,
    id: u32,
    state: Rc<RefCell<AudioState>>,
) -> Result<MetadataProxy, pw::Error> {
    let metadata: pw::metadata::Metadata = registry.bind(&pw::registry::GlobalObject {
        id,
        permissions: pw::permissions::PermissionFlags::empty(),
        type_: ObjectType::Metadata,
        version: 3,
        props: None::<&pw::spa::utils::dict::DictRef>,
    })?;

    let listener = metadata
        .add_listener_local()
        .property(move |_subject, key, _type, value| {
            // `None` for key means "all properties cleared" — reset both
            // defaults. `None` for value means "delete this property".
            let Some(key) = key else {
                let mut s = state.borrow_mut();
                let changed_any = s.default_sink_name.is_some()
                    || s.default_source_name.is_some();
                s.default_sink_name = None;
                s.default_source_name = None;
                if changed_any {
                    emit_snapshots(&mut s);
                }
                return 0;
            };
            match key {
                "default.audio.sink" => {
                    let new_name = value.and_then(parse_default_name);
                    let mut s = state.borrow_mut();
                    if s.default_sink_name != new_name {
                        s.default_sink_name = new_name;
                        emit_snapshots(&mut s);
                    }
                }
                "default.audio.source" => {
                    let new_name = value.and_then(parse_default_name);
                    let mut s = state.borrow_mut();
                    if s.default_source_name != new_name {
                        s.default_source_name = new_name;
                        emit_snapshots(&mut s);
                    }
                }
                _ => {} // ignore unrelated metadata keys
            }
            0
        })
        .register();

    Ok(MetadataProxy {
        proxy: metadata,
        listener,
    })
}

/// Extract `node.name` from a `default.audio.{sink,source}` metadata
/// payload. `PipeWire` encodes these as JSON: `{"name":"<node.name>"}`.
/// Returns `None` if the JSON is malformed or missing the `name` field —
/// safer than guessing, the previous default just stays in place.
fn parse_default_name(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    v.get("name")?.as_str().map(str::to_owned)
}

/// Dispatch a [`Command`] arriving from the tokio side. Resolves the target
/// node, builds a Props pod (Phase 3 — `channelVolumes` or `mute`), and
/// calls `node.set_param`. Runs on the pw-loop thread.
fn handle_command(cmd: Command, state: &Rc<RefCell<AudioState>>) {
    match cmd {
        Command::SetSinkVolume { name, linear } => {
            apply_volume_by_name(state, &name, AudioRole::Sink, linear);
        }
        Command::SetSourceVolume { name, linear } => {
            apply_volume_by_name(state, &name, AudioRole::Source, linear);
        }
        Command::SetStreamVolume { id, linear } => {
            apply_volume_by_id(state, id, linear);
        }
        Command::SetSinkMute { name, mute } => {
            apply_mute_by_name(state, &name, AudioRole::Sink, mute);
        }
        Command::SetSourceMute { name, mute } => {
            apply_mute_by_name(state, &name, AudioRole::Source, mute);
        }
        Command::SetStreamMute { id, mute } => {
            apply_mute_by_id(state, id, mute);
        }
        Command::SetDefaultSink { name } => {
            write_default(state, "default.audio.sink", &name);
        }
        Command::SetDefaultSource { name } => {
            write_default(state, "default.audio.source", &name);
        }
    }
}

/// Resolve a node by `node.name` and target role, then call the closure
/// with its pipewire id and current channel count. The closure builds the
/// appropriate Props pod and calls `set_param`. Returns silently if the
/// node isn't in the cache yet (e.g. a stale name from a UI race).
fn with_named_node<F>(
    state: &Rc<RefCell<AudioState>>,
    name: &str,
    role: AudioRole,
    f: F,
) where
    F: FnOnce(&pw::node::Node, usize),
{
    let s = state.borrow();
    let Some((id, entry)) = s
        .nodes
        .iter()
        .find(|(_, e)| e.role == role && e.name == name)
    else {
        tracing::debug!(name, ?role, "audio_native: target node not in cache");
        return;
    };
    let channels = entry.channel_volumes.len();
    let Some(proxy) = s.proxies.get(id) else {
        tracing::debug!(name, ?role, "audio_native: target node has no proxy");
        return;
    };
    f(&proxy.proxy, channels);
}

fn with_id_node<F>(state: &Rc<RefCell<AudioState>>, id: u32, f: F)
where
    F: FnOnce(&pw::node::Node, usize),
{
    let s = state.borrow();
    let Some(entry) = s.nodes.get(&id) else {
        tracing::debug!(id, "audio_native: target stream not in cache");
        return;
    };
    let channels = entry.channel_volumes.len();
    let Some(proxy) = s.proxies.get(&id) else {
        tracing::debug!(id, "audio_native: target stream has no proxy");
        return;
    };
    f(&proxy.proxy, channels);
}

fn apply_volume_by_name(
    state: &Rc<RefCell<AudioState>>,
    name: &str,
    role: AudioRole,
    linear: f64,
) {
    with_named_node(state, name, role, |node, channels| {
        send_volume(node, channels, linear);
    });
}

fn apply_volume_by_id(state: &Rc<RefCell<AudioState>>, id: u32, linear: f64) {
    with_id_node(state, id, |node, channels| {
        send_volume(node, channels, linear);
    });
}

fn apply_mute_by_name(
    state: &Rc<RefCell<AudioState>>,
    name: &str,
    role: AudioRole,
    mute: bool,
) {
    with_named_node(state, name, role, |node, _channels| send_mute(node, mute));
}

fn apply_mute_by_id(state: &Rc<RefCell<AudioState>>, id: u32, mute: bool) {
    with_id_node(state, id, |node, _channels| send_mute(node, mute));
}

/// Write `default.audio.{sink,source}` to the `default` Metadata. The C
/// API expects the value as `Spa:String:JSON` formatted as
/// `{"name":"<node.name>"}`. Subject 0 targets "any" (global) scope —
/// matches what `wpctl set-default` does. Silently no-ops if the
/// `default` metadata hasn't been seen yet (e.g. the loop hasn't
/// reached the metadata global yet on startup).
fn write_default(state: &Rc<RefCell<AudioState>>, key: &str, name: &str) {
    let s = state.borrow();
    let Some(meta) = s.metadata_default.as_ref() else {
        tracing::warn!(
            key,
            name,
            "audio_native: write_default before default metadata bound",
        );
        return;
    };
    let value = serde_json::json!({ "name": name }).to_string();
    meta.proxy
        .set_property(0, key, Some("Spa:String:JSON"), Some(&value));
}

/// Build a `SPA_TYPE_OBJECT_Props` pod carrying just `SPA_PROP_channelVolumes`
/// (one float per channel, all set to `linear`) and dispatch it via
/// `node.set_param`. If `channels == 0` the cache hasn't seen a Props event
/// yet — silently skip rather than publishing a mono array that would
/// clobber a stereo sink's layout.
// `linear` gain is in [0,1]; f32 is PipeWire's channelVolumes element type.
#[allow(clippy::cast_possible_truncation)]
fn send_volume(node: &pw::node::Node, channels: usize, linear: f64) {
    if channels == 0 {
        tracing::debug!("audio_native: skip set_volume — channel count unknown");
        return;
    }
    let channel_volumes: Vec<f32> = vec![linear as f32; channels];
    let pod = build_props_pod(Some(channel_volumes), None);
    let Some(pod) = pod else {
        tracing::warn!("audio_native: failed to build volume pod");
        return;
    };
    let Some(pod_ref) = pw::spa::pod::Pod::from_bytes(&pod) else {
        tracing::warn!("audio_native: built pod bytes failed Pod::from_bytes");
        return;
    };
    node.set_param(pw::spa::param::ParamType::Props, 0, pod_ref);
}

/// Build a Props pod carrying just `SPA_PROP_mute` and dispatch.
fn send_mute(node: &pw::node::Node, mute: bool) {
    let pod = build_props_pod(None, Some(mute));
    let Some(pod) = pod else {
        tracing::warn!("audio_native: failed to build mute pod");
        return;
    };
    let Some(pod_ref) = pw::spa::pod::Pod::from_bytes(&pod) else {
        tracing::warn!("audio_native: built pod bytes failed Pod::from_bytes");
        return;
    };
    node.set_param(pw::spa::param::ParamType::Props, 0, pod_ref);
}

/// Serialize a `SPA_TYPE_OBJECT_Props` pod with whichever of channelVolumes
/// and mute are supplied. Returns the raw byte buffer, which the caller
/// wraps with `Pod::from_bytes` before passing to `set_param`. Both fields
/// optional so we can issue volume-only and mute-only updates without
/// clobbering the other.
fn build_props_pod(
    channel_volumes: Option<Vec<f32>>,
    mute: Option<bool>,
) -> Option<Vec<u8>> {
    use pw::spa::pod::{serialize::PodSerializer, Object, Property, Value, ValueArray};
    let mut properties = Vec::new();
    if let Some(mute) = mute {
        properties.push(Property::new(
            pw::spa::sys::SPA_PROP_mute,
            Value::Bool(mute),
        ));
    }
    if let Some(cv) = channel_volumes {
        properties.push(Property::new(
            pw::spa::sys::SPA_PROP_channelVolumes,
            Value::ValueArray(ValueArray::Float(cv)),
        ));
    }
    if properties.is_empty() {
        return None;
    }
    let obj = Value::Object(Object {
        type_: pw::spa::sys::SPA_TYPE_OBJECT_Props,
        id: pw::spa::sys::SPA_PARAM_Props,
        properties,
    });
    let mut buf = Vec::new();
    PodSerializer::serialize(std::io::Cursor::new(&mut buf), &obj).ok()?;
    Some(buf)
}

/// Decode a Props `spa_pod` payload, extracting `channelVolumes` and `mute`
/// when present. Returns `None` only if the pod itself is unparseable; a
/// successful parse with neither key present returns `Some((None, None))`,
/// which the caller treats as a no-op update.
fn decode_props(bytes: &[u8]) -> Option<(Option<Vec<f32>>, Option<bool>)> {
    use pw::spa::pod::{deserialize::PodDeserializer, Value, ValueArray};
    let (_rest, value) = PodDeserializer::deserialize_from::<Value>(bytes).ok()?;
    let Value::Object(obj) = value else {
        return Some((None, None));
    };
    let mut channel_volumes = None;
    let mut mute = None;
    for prop in obj.properties {
        match prop.key {
            // SPA_PROP_channelVolumes — array of per-channel linear gains.
            // 65544: see libspa-sys generated bindings.
            65544 => {
                if let Value::ValueArray(ValueArray::Float(v)) = prop.value {
                    channel_volumes = Some(v);
                }
            }
            // SPA_PROP_mute. 65540 per libspa-sys.
            65540 => {
                if let Value::Bool(b) = prop.value {
                    mute = Some(b);
                }
            }
            _ => {}
        }
    }
    Some((channel_volumes, mute))
}

/// Walk the node cache and push fresh snapshots into the four `Mutable`s
/// (sinks, sources, playback streams, record streams) plus the default-sink
/// `Volume`. Compares against the prior snapshot to skip no-op `set()`s.
///
/// `is_default` is set against `state.default_sink_name` /
/// `default_source_name` populated by the Metadata listener. Stream
/// routing fields (`sink_id`, `source_id`) are `0` placeholders until
/// Phase 5 walks Link globals.
fn emit_snapshots(state: &mut AudioState) {
    let mut sinks: Vec<Sink> = Vec::new();
    let mut sources: Vec<Source> = Vec::new();
    let mut streams: Vec<PlaybackStream> = Vec::new();
    let mut record_streams: Vec<RecordStream> = Vec::new();

    let default_sink_name = state.default_sink_name.as_deref();
    let default_source_name = state.default_source_name.as_deref();

    for (id, entry) in &state.nodes {
        let volume = avg_volume(&entry.channel_volumes);
        match entry.role {
            AudioRole::Sink => sinks.push(Sink {
                id: *id,
                name: entry.name.clone(),
                description: entry.description.clone(),
                volume,
                muted: entry.mute,
                is_default: default_sink_name == Some(entry.name.as_str()),
            }),
            AudioRole::Source => sources.push(Source {
                id: *id,
                name: entry.name.clone(),
                description: entry.description.clone(),
                volume,
                muted: entry.mute,
                is_default: default_source_name == Some(entry.name.as_str()),
            }),
            AudioRole::OutputStream => {
                // Playback stream → sink: the stream is the link's
                // *output* node, the sink is the link's *input* node.
                // 0 if no link found (rare: brief transitional state).
                let sink_id = resolve_link_dest(&state.links, *id);
                streams.push(PlaybackStream {
                    id: *id,
                    app_name: entry.app_name.clone().unwrap_or_default(),
                    sink_id,
                    volume,
                    muted: entry.mute,
                });
            }
            AudioRole::InputStream => {
                // Record stream → source: the stream is the link's
                // *input* node, the source is the link's *output* node.
                let source_id = resolve_link_source(&state.links, *id);
                record_streams.push(RecordStream {
                    id: *id,
                    app_name: entry.app_name.clone().unwrap_or_default(),
                    source_id,
                    volume,
                    muted: entry.mute,
                });
            }
        }
    }

    // Stable ordering by id so consumers don't see synthetic reorderings
    // when HashMap iteration order shifts. Matches the natural pactl id
    // ordering closely enough for the audio modal.
    sinks.sort_by_key(|s| s.id);
    sources.sort_by_key(|s| s.id);
    streams.sort_by_key(|s| s.id);
    record_streams.sort_by_key(|s| s.id);

    let default_volume = sinks
        .iter()
        .find(|s| s.is_default)
        .map_or(Volume::default(), |s| Volume {
            linear: s.volume,
            muted: s.muted,
        });

    if default_volume != state.last_sink_volume {
        state.last_sink_volume = default_volume;
        state.handles.sink.set(default_volume);
    }
    if sinks != state.last_sinks {
        state.last_sinks.clone_from(&sinks);
        state.handles.sinks.set(sinks);
    }
    if sources != state.last_sources {
        state.last_sources.clone_from(&sources);
        state.handles.sources.set(sources);
    }
    if streams != state.last_streams {
        state.last_streams.clone_from(&streams);
        state.handles.streams.set(streams);
    }
    if record_streams != state.last_record_streams {
        state.last_record_streams.clone_from(&record_streams);
        state.handles.record_streams.set(record_streams);
    }
}

/// Resolve a playback stream's target sink id by scanning the link cache
/// for an edge whose `output_node` matches the stream. Returns the input
/// (sink) side of the first match. `PipeWire` usually creates one link per
/// stereo pair, so any of them works.
///
/// A stream typically has multiple ports → multiple links, but every
/// link goes to the same sink, so the first match is correct.
fn resolve_link_dest(links: &HashMap<u32, LinkEdge>, stream_id: u32) -> u32 {
    links
        .values()
        .find(|e| e.output_node == stream_id)
        .map_or(0, |e| e.input_node)
}

/// Mirror of [`resolve_link_dest`] for record streams: the stream is the
/// link's *input*, so the source id is `output_node`.
fn resolve_link_source(links: &HashMap<u32, LinkEdge>, stream_id: u32) -> u32 {
    links
        .values()
        .find(|e| e.input_node == stream_id)
        .map_or(0, |e| e.output_node)
}

/// Average of per-channel linear gains. `PipeWire` spec doesn't require all
/// channels to agree (you can pan via uneven channelVolumes), but pactl's
/// historical convention — which the UI is calibrated against — reports
/// the first channel's value. Averaging is friendlier when the UI shows
/// a single slider for a stereo sink: a 100%/0% pair reads 50% instead of
/// 100%, matching what the user perceives. Empty array → 0.0.
// Channel count is a handful of elements; usize→f64 loses no precision here.
#[allow(clippy::cast_precision_loss)]
fn avg_volume(channels: &[f32]) -> f64 {
    if channels.is_empty() {
        return 0.0;
    }
    let sum: f64 = channels.iter().map(|v| f64::from(*v)).sum();
    sum / channels.len() as f64
}

/// Returns the audio service to register with the hytte runtime.
#[must_use]
pub fn service() -> PipewireService {
    PipewireService
}

// ── Mutation surface ──────────────────────────────────────────────────────
//
// Mirrors `super::pipewire`'s public mutation API so Phase 6 cutover is a
// `pub use audio_native::*` swap. All eight functions are fire-and-forget
// and silently drop the command if the audio service hasn't started yet
// (loop hasn't installed `COMMAND_TX`) or the receiver has gone away
// (process tearing down). Real failures land in tracing.

/// Send a command on the loop's channel, or warn if the service hasn't
/// started yet. Helper for the eight wrappers below.
fn send_command(cmd: Command) {
    let Some(tx) = COMMAND_TX.get() else {
        tracing::warn!("audio_native: command before service started");
        return;
    };
    if tx.send(cmd).is_err() {
        tracing::warn!("audio_native: send_command failed (receiver dropped)");
    }
}

#[allow(dead_code)] // Phase 6 swaps this in for super::pipewire's version
pub fn set_sink_volume(name: &str, linear: f64) {
    send_command(Command::SetSinkVolume {
        name: name.to_string(),
        linear,
    });
}

#[allow(dead_code)]
pub fn set_source_volume(name: &str, linear: f64) {
    send_command(Command::SetSourceVolume {
        name: name.to_string(),
        linear,
    });
}

#[allow(dead_code)]
pub fn set_stream_volume(id: u32, linear: f64) {
    send_command(Command::SetStreamVolume { id, linear });
}

#[allow(dead_code)]
pub fn set_sink_mute(name: &str, mute: bool) {
    send_command(Command::SetSinkMute {
        name: name.to_string(),
        mute,
    });
}

#[allow(dead_code)]
pub fn set_source_mute(name: &str, mute: bool) {
    send_command(Command::SetSourceMute {
        name: name.to_string(),
        mute,
    });
}

#[allow(dead_code)]
pub fn set_stream_mute(id: u32, mute: bool) {
    send_command(Command::SetStreamMute { id, mute });
}

#[allow(dead_code)]
pub fn set_default_sink(name: &str) {
    send_command(Command::SetDefaultSink {
        name: name.to_string(),
    });
}

#[allow(dead_code)]
pub fn set_default_source(name: &str) {
    send_command(Command::SetDefaultSource {
        name: name.to_string(),
    });
}

/// Set volume on whichever sink is currently the default. The default-sink
/// resolution still lives in Phase 4's Metadata path, so for Phase 3 this
/// reads the last default-sink name out of the cached `sinks()` snapshot.
/// Once Phase 4 lands, `is_default` actually gets set and this becomes
/// reliable.
#[allow(dead_code)]
pub fn set_volume(linear: f64) {
    let name = registry::with(|r| {
        r.get::<PipewireHandles>()
            .and_then(|h| {
                h.sinks
                    .lock_ref()
                    .iter()
                    .find(|s| s.is_default)
                    .map(|s| s.name.clone())
            })
    });
    if let Some(name) = name {
        set_sink_volume(&name, linear);
    } else {
        tracing::debug!("audio_native: set_volume without default sink");
    }
}

/// Toggle mute on the current default sink. Same default-resolution caveat
/// as [`set_volume`] until Phase 4 lands.
#[allow(dead_code)]
pub fn toggle_mute() {
    let target = registry::with(|r| {
        r.get::<PipewireHandles>().and_then(|h| {
            h.sinks
                .lock_ref()
                .iter()
                .find(|s| s.is_default)
                .map(|s| (s.name.clone(), s.muted))
        })
    });
    if let Some((name, muted)) = target {
        set_sink_mute(&name, !muted);
    } else {
        tracing::debug!("audio_native: toggle_mute without default sink");
    }
}

/// Choose the user-facing app name for a stream's props dict. Mirrors the
/// fallback chain `super::pipewire::pick_app_name` uses on `pactl list
/// sink-inputs` output — Spotify in particular publishes only `node.name`
/// over the pipewire-pulse compat layer, and several generic placeholders
/// must be filtered so a useful `media.name` further down the list wins.
fn pick_app_name(props: &pw::spa::utils::dict::DictRef) -> String {
    const KEYS: &[&str] = &[
        "application.name",
        "node.description",
        "node.nick",
        "node.name",
        "application.process.binary",
        "media.name",
    ];
    const GENERIC: &[&str] = &[
        "audio-src",
        "audio-sink",
        "input-port",
        "output-port",
        "alsa-sink",
        "alsa-source",
        "Stream",
        "Loopback",
    ];
    for key in KEYS {
        if let Some(v) = props.get(key) {
            let t = v.trim();
            if !t.is_empty() && !GENERIC.iter().any(|g| g.eq_ignore_ascii_case(t)) {
                return t.to_string();
            }
        }
    }
    "Stream".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_role_classifies_the_four_relevant_classes() {
        assert_eq!(
            AudioRole::from_media_class("Audio/Sink"),
            Some(AudioRole::Sink),
        );
        assert_eq!(
            AudioRole::from_media_class("Audio/Source"),
            Some(AudioRole::Source),
        );
        assert_eq!(
            AudioRole::from_media_class("Stream/Output/Audio"),
            Some(AudioRole::OutputStream),
        );
        assert_eq!(
            AudioRole::from_media_class("Stream/Input/Audio"),
            Some(AudioRole::InputStream),
        );
    }

    #[test]
    fn audio_role_ignores_non_audio_classes() {
        // Cameras, MIDI, virtual surface nodes — anything that isn't one
        // of the four explicit classes must return None so the registry
        // walker skips it instead of mis-classifying as a sink.
        assert_eq!(AudioRole::from_media_class("Video/Source"), None);
        assert_eq!(AudioRole::from_media_class("Midi/Bridge"), None);
        assert_eq!(AudioRole::from_media_class(""), None);
        assert_eq!(AudioRole::from_media_class("Audio/Duplex"), None);
    }

    #[test]
    #[allow(clippy::float_cmp)] // exact 0.0 sentinel for the empty-input case
    fn avg_volume_empty_is_zero() {
        // A node we haven't received Props for yet has an empty
        // channelVolumes Vec. The UI reads `volume` and divides by
        // implicit ranges; producing 0.0 keeps it safely off the rails.
        assert_eq!(avg_volume(&[]), 0.0);
    }

    #[test]
    fn avg_volume_mono_passes_through() {
        // Mono sink: one channel at 0.5 linear → reported 0.5. Matches
        // wpctl's first-channel convention, which the existing pactl
        // backend also produces.
        assert!((avg_volume(&[0.5]) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn avg_volume_stereo_averages() {
        // L=1.0, R=0.0 (extreme pan) → 0.5 on the single-slider UI.
        // Matches the user's perception better than first-channel-only,
        // which would read 100% while half the speakers were silent.
        assert!((avg_volume(&[1.0, 0.0]) - 0.5).abs() < 1e-9);
    }

    /// Locks the public Props key codes against drift in libspa-sys: if
    /// these constants ever change upstream, the decoder will silently
    /// stop seeing volume/mute updates. Asserting against the raw values
    /// (rather than the `spa_sys` constants) means a libspa-sys version
    /// bump that changes them shows up here as a hard failure, not as
    /// "the slider just stopped moving".
    #[test]
    fn spa_prop_constants_match_bindings() {
        assert_eq!(pw::spa::sys::SPA_PROP_channelVolumes, 65544);
        assert_eq!(pw::spa::sys::SPA_PROP_mute, 65540);
    }

    /// Round-trip: build a volume-only Props pod, deserialize it back, and
    /// confirm the decoder sees exactly the channelVolumes we put in.
    /// Guards both the serializer's Object-encoding and the decoder's
    /// pattern-match arms — a single broken byte in either side would
    /// silently make `set_volume` a no-op.
    #[test]
    fn build_props_pod_volume_roundtrips() {
        let bytes = build_props_pod(Some(vec![0.5, 0.5]), None).expect("pod bytes");
        let (cv, mute) = decode_props(&bytes).expect("decode");
        assert_eq!(cv, Some(vec![0.5, 0.5]));
        assert_eq!(mute, None);
    }

    /// Same round-trip for mute. Bool encoding lives in a different code
    /// path inside the `spa_pod` format (Bool vs Float-array), so the two
    /// guards aren't redundant.
    #[test]
    fn build_props_pod_mute_roundtrips() {
        let bytes = build_props_pod(None, Some(true)).expect("pod bytes");
        let (cv, mute) = decode_props(&bytes).expect("decode");
        assert_eq!(cv, None);
        assert_eq!(mute, Some(true));
    }

    /// Combined volume+mute pod is the common path for an idempotent
    /// "snap the sink to this state" — verify both keys survive together.
    #[test]
    fn build_props_pod_both_roundtrips() {
        let bytes = build_props_pod(Some(vec![0.3]), Some(false)).expect("pod bytes");
        let (cv, mute) = decode_props(&bytes).expect("decode");
        assert_eq!(cv, Some(vec![0.3]));
        assert_eq!(mute, Some(false));
    }

    /// An empty Props payload is a degenerate request — neither key is
    /// supplied. The builder returns None so the caller doesn't pay for
    /// an empty-Object `set_param` round-trip.
    #[test]
    fn build_props_pod_empty_returns_none() {
        assert!(build_props_pod(None, None).is_none());
    }

    /// `default.audio.sink` payloads from `PipeWire` come as a JSON object
    /// with a `name` key. Verify the canonical extraction.
    #[test]
    fn parse_default_name_canonical() {
        let p = parse_default_name(r#"{"name":"alsa_output.pci-0000_00_1f.3.analog-stereo"}"#);
        assert_eq!(
            p.as_deref(),
            Some("alsa_output.pci-0000_00_1f.3.analog-stereo"),
        );
    }

    /// JSON with extra keys (`PipeWire` sometimes adds `value` or hints).
    /// We only care about `name`.
    #[test]
    fn parse_default_name_ignores_extra_keys() {
        let p = parse_default_name(r#"{"name":"sink-a","extra":42}"#);
        assert_eq!(p.as_deref(), Some("sink-a"));
    }

    /// Malformed JSON or missing `name` → None so the previous default
    /// stays cached. Safer than guessing.
    #[test]
    fn parse_default_name_rejects_malformed() {
        assert!(parse_default_name("not json").is_none());
        assert!(parse_default_name(r#"{"id":17}"#).is_none());
        assert!(parse_default_name(r#"{"name":42}"#).is_none());
    }

    /// Playback-stream routing: link goes stream → sink, so the link's
    /// `output_node` is the stream and `input_node` is the sink.
    #[test]
    fn resolve_link_dest_finds_sink() {
        let mut links = HashMap::new();
        links.insert(
            100,
            LinkEdge {
                output_node: 42,
                input_node: 10,
            },
        );
        assert_eq!(resolve_link_dest(&links, 42), 10);
    }

    /// Record-stream routing reverses: link goes source → stream, so the
    /// stream is the link's `input_node`.
    #[test]
    fn resolve_link_source_finds_source() {
        let mut links = HashMap::new();
        links.insert(
            100,
            LinkEdge {
                output_node: 5,
                input_node: 99,
            },
        );
        assert_eq!(resolve_link_source(&links, 99), 5);
    }

    /// No matching link → 0 sentinel, so the audio modal can show the
    /// stream without crashing the routing path. Stale state is allowed
    /// in the brief window between a link removal and the corresponding
    /// stream's WindowsChanged-equivalent event.
    #[test]
    fn resolve_link_returns_zero_when_no_match() {
        let links = HashMap::new();
        assert_eq!(resolve_link_dest(&links, 42), 0);
        assert_eq!(resolve_link_source(&links, 42), 0);
    }
}
