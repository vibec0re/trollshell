//! `PipeWire` mainloop, registry walker, and command dispatcher.
//!
//! This module owns the dedicated `hytte-audio-pw` thread, the
//! [`COMMAND_TX`] static sender, and all the wiring that bridges the
//! `PipeWire` C event loop to the `futures-signals` `Mutable`s in
//! [`AudioState`].

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::OnceLock;
use std::thread;

use futures_signals::signal::Mutable;
use pipewire as pw;
use pw::types::ObjectType;

use super::super::pipewire::{
    AudioSpectrum, PipewireHandles, PlaybackStream, RecordStream, Sink, Source, Volume,
};
use super::graph::{resolve_link_dest, resolve_link_source};
use super::pod::{avg_volume, build_props_pod, decode_props, parse_default_name, pick_app_name};
use super::spectrum::SpectrumUserData;
use super::types::{
    AudioRole, AudioState, Command, LinkEdge, MetadataProxy, NodeEntry, NodeProxy, SpectrumCapture,
    StateRef, clone_handles,
};

/// Sender shared across all callers. Populated by [`spawn_mainloop`] before
/// the loop runs; mutation fns read it with `.get()`. A `OnceLock` keeps
/// this thread-safe without requiring a `Mutex`, and an inert clone after
/// `spawn_mainloop` returns is enough to send commands from any thread.
pub(super) static COMMAND_TX: OnceLock<pw::channel::Sender<Command>> = OnceLock::new();

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
pub(super) fn spawn_mainloop(handles: PipewireHandles) {
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
pub(super) fn run_once(
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

    // Audio spectrum capture tap (#405): a capture stream on the default sink's
    // monitor, built **inactive** so an idle desktop pays nothing. The plugin
    // host flips it active via `Command::SetSpectrumActive` once a plugin
    // subscribes `StateKey::AudioSpectrum`. A build failure (no monitor, older
    // daemon) is logged and simply leaves the spectrum dark — every other audio
    // feature is unaffected.
    let spectrum_out = state.borrow().handles.spectrum.clone();
    if let Some(capture) = build_spectrum_capture(&core, spectrum_out) {
        state.borrow_mut().spectrum_capture = Some(capture);
    } else {
        tracing::warn!("audio_native: spectrum capture unavailable this session");
    }

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
                        .get("node.description")
                        .map_or_else(|| name.clone(), str::to_owned);
                    let app_name = matches!(role, AudioRole::OutputStream | AudioRole::InputStream)
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
                    match bind_node_for_params(&registry_for_bind, obj.id, Rc::clone(&state_add)) {
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
                    tracing::trace!(id = obj.id, out, input, "audio_native: + link",);
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
    state: StateRef,
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
                    && cv != entry.channel_volumes
                {
                    entry.channel_volumes = cv;
                    changed = true;
                }
                if let Some(m) = mute
                    && m != entry.mute
                {
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
    state: StateRef,
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
                let changed_any = s.default_sink_name.is_some() || s.default_source_name.is_some();
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

/// Dispatch a [`Command`] arriving from the tokio side. Resolves the target
/// node, builds a Props pod (Phase 3 — `channelVolumes` or `mute`), and
/// calls `node.set_param`. Runs on the pw-loop thread.
fn handle_command(cmd: Command, state: &StateRef) {
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
        Command::SetSpectrumActive { active } => {
            set_spectrum_active(state, active);
        }
    }
}

/// Toggle the audio spectrum capture tap active/inactive (#405). Silently
/// no-ops if the capture stream failed to build this session.
fn set_spectrum_active(state: &StateRef, active: bool) {
    let s = state.borrow();
    let Some(capture) = s.spectrum_capture.as_ref() else {
        tracing::debug!(
            active,
            "audio_native: spectrum capture not built; ignoring toggle"
        );
        return;
    };
    if let Err(e) = capture.stream.set_active(active) {
        tracing::warn!(error = ?e, active, "audio_native: set spectrum active failed");
    } else {
        tracing::debug!(active, "audio_native: spectrum capture toggled");
    }
}

/// Resolve a node by `node.name` and target role, then call the closure
/// with its pipewire id and current channel count. The closure builds the
/// appropriate Props pod and calls `set_param`. Returns silently if the
/// node isn't in the cache yet (e.g. a stale name from a UI race).
fn with_named_node<F>(state: &StateRef, name: &str, role: AudioRole, f: F)
where
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

fn with_id_node<F>(state: &StateRef, id: u32, f: F)
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

fn apply_volume_by_name(state: &StateRef, name: &str, role: AudioRole, linear: f64) {
    with_named_node(state, name, role, |node, channels| {
        send_volume(node, channels, linear);
    });
}

fn apply_volume_by_id(state: &StateRef, id: u32, linear: f64) {
    with_id_node(state, id, |node, channels| {
        send_volume(node, channels, linear);
    });
}

fn apply_mute_by_name(state: &StateRef, name: &str, role: AudioRole, mute: bool) {
    with_named_node(state, name, role, |node, _channels| send_mute(node, mute));
}

fn apply_mute_by_id(state: &StateRef, id: u32, mute: bool) {
    with_id_node(state, id, |node, _channels| send_mute(node, mute));
}

/// Write `default.audio.{sink,source}` to the `default` Metadata. The C
/// API expects the value as `Spa:String:JSON` formatted as
/// `{"name":"<node.name>"}`. Subject 0 targets "any" (global) scope —
/// matches what `wpctl set-default` does. Silently no-ops if the
/// `default` metadata hasn't been seen yet (e.g. the loop hasn't
/// reached the metadata global yet on startup).
fn write_default(state: &StateRef, key: &str, name: &str) {
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
fn send_volume(node: &pw::node::Node, channels: usize, linear: f64) {
    if channels == 0 {
        tracing::debug!("audio_native: skip set_volume — channel count unknown");
        return;
    }
    let channel_volumes: Vec<f32> = vec![crate::cast::f64_to_f32_gain(linear); channels];
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

/// Walk the node cache and push fresh snapshots into the four `Mutable`s
/// (sinks, sources, playback streams, record streams) plus the default-sink
/// `Volume`. Compares against the prior snapshot to skip no-op `set()`s.
///
/// `is_default` is set against `state.default_sink_name` /
/// `default_source_name` populated by the Metadata listener. Stream
/// routing fields (`sink_id`, `source_id`) are `0` placeholders until
/// Phase 5 walks Link globals.
pub(super) fn emit_snapshots(state: &mut AudioState) {
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

/// Build the audio spectrum capture stream (#405): an F32 input stream on the
/// **default sink's monitor** (`stream.capture.sink = true` + autoconnect makes
/// it follow the default sink), connected **inactive**. The `param_changed`
/// callback learns the negotiated rate/channels; the `process` callback feeds
/// samples through the [`super::spectrum::Analyzer`] and publishes each finished
/// `{peak, bins}` frame to the `out` handle. Returns `None` (logged by the
/// caller) if any step fails, leaving the feature dark without disturbing the
/// rest of the audio service.
///
/// The `process` callback runs on this loop's own thread (no `RT_PROCESS`
/// flag), the same thread that already pushes `emit_snapshots`, so writing the
/// `Mutable` from it is consistent with the rest of the backend.
fn build_spectrum_capture(
    core: &pw::core::CoreRc,
    out: Mutable<Option<AudioSpectrum>>,
) -> Option<SpectrumCapture> {
    let props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Music",
        // Capture the monitor of a sink rather than a real source; with
        // autoconnect and no explicit target this follows the *default* sink.
        *pw::keys::STREAM_CAPTURE_SINK => "true",
        *pw::keys::NODE_NAME => "trollshell-spectrum",
    };

    let stream = pw::stream::StreamRc::new(core.clone(), "trollshell-spectrum", props)
        .map_err(|e| tracing::warn!(error = ?e, "audio_native: spectrum stream create failed"))
        .ok()?;

    let listener = stream
        .add_local_listener_with_user_data(SpectrumUserData::new(out))
        .param_changed(|_stream, ud, id, param| {
            let Some(param) = param else { return };
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, media_subtype)) = pw::spa::param::format_utils::parse_format(param)
            else {
                return;
            };
            if media_type != pw::spa::param::format::MediaType::Audio
                || media_subtype != pw::spa::param::format::MediaSubtype::Raw
            {
                return;
            }
            if ud.format.parse(param).is_ok() {
                ud.analyzer.set_rate(ud.format.rate());
            }
        })
        .process(|stream, ud| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else {
                return;
            };
            let channels = usize::try_from(ud.format.channels()).unwrap_or(1).max(1);
            let size = usize::try_from(data.chunk().size()).unwrap_or(0);
            let Some(bytes) = data.data() else {
                return;
            };
            let usable = size.min(bytes.len());
            if let Some(spectrum) = ud.analyzer.push_bytes(&bytes[..usable], channels) {
                ud.out.set(Some(spectrum));
            }
        })
        .register()
        .map_err(|e| tracing::warn!(error = ?e, "audio_native: spectrum listener failed"))
        .ok()?;

    let format_bytes = build_enum_format_pod()?;
    let pod = pw::spa::pod::Pod::from_bytes(&format_bytes)?;
    let mut params = [pod];
    stream
        .connect(
            pw::spa::utils::Direction::Input,
            None,
            pw::stream::StreamFlags::AUTOCONNECT
                | pw::stream::StreamFlags::MAP_BUFFERS
                | pw::stream::StreamFlags::INACTIVE,
            &mut params,
        )
        .map_err(|e| tracing::warn!(error = ?e, "audio_native: spectrum connect failed"))
        .ok()?;

    tracing::debug!("audio_native: spectrum capture built (inactive)");
    Some(SpectrumCapture { listener, stream })
}

/// Serialize a one-value `EnumFormat` pod requesting F32 (little-endian) raw
/// audio, leaving rate and channels empty so the graph's native values are
/// accepted. Mirrors the pipewire-rs audio-capture example.
fn build_enum_format_pod() -> Option<Vec<u8>> {
    let mut audio_info = pw::spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(pw::spa::param::audio::AudioFormat::F32LE);
    let obj = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    let mut buf = Vec::new();
    pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(&mut buf),
        &pw::spa::pod::Value::Object(obj),
    )
    .ok()?;
    Some(buf)
}

/// Send a command on the loop's channel, or warn if the service hasn't
/// started yet. Helper for the eight wrappers below.
pub(super) fn send_command(cmd: Command) {
    let Some(tx) = COMMAND_TX.get() else {
        tracing::warn!("audio_native: command before service started");
        return;
    };
    if tx.send(cmd).is_err() {
        tracing::warn!("audio_native: send_command failed (receiver dropped)");
    }
}
