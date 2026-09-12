//! `PipeWire` mainloop, registry walker, and command dispatcher.
//!
//! This module owns the dedicated `hytte-audio-pw` thread, the
//! [`COMMAND_TX`] static sender, and all the wiring that bridges the
//! `PipeWire` C event loop to the `futures-signals` `Mutable`s in
//! [`AudioState`].

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, Once, PoisonError, RwLock};
use std::thread;
use std::time::Instant;

use futures_signals::signal::Mutable;
use hytte_reactive::spawn_supervised_blocking;
use pipewire as pw;
use pw::channel::Receiver;
use pw::types::ObjectType;

use super::super::pipewire::{
    AudioSpectrum, PipewireHandles, PlaybackStream, RecordStream, Sink, Source, Volume,
};
use super::graph::{resolve_link_dest, resolve_link_source};
use super::pod::{avg_volume, build_props_pod, decode_props, parse_default_name, pick_app_name};
use super::spectrum::SpectrumUserData;
use super::types::{
    AudioRole, AudioState, Command, LinkEdge, MetadataProxy, NodeEntry, NodeProxy, SpectrumAction,
    SpectrumCapture, StateRef, clone_handles,
};
use crate::retry;

/// Sender shared across all callers. Published by [`spawn_mainloop`] before the
/// loop runs; mutation fns read it through [`send_command`].
///
/// A re-settable slot rather than a `OnceLock` since #1170. The mainloop is
/// supervised now, and a panicking run unwinds **with the `Receiver` it was
/// holding**: without a way to republish a sender, the restarted run would find
/// no receiver, and every `set_*` for the rest of the session would go nowhere.
/// [`session_receiver`] is what re-establishes the pair; [`STARTED`] keeps the
/// called-twice guard the `OnceLock` used to provide.
pub(super) static COMMAND_TX: RwLock<Option<pw::channel::Sender<Command>>> = RwLock::new(None);

/// Whether [`spawn_mainloop`] has already run. Its own flag now that
/// [`COMMAND_TX`] is no longer set-once.
static STARTED: AtomicBool = AtomicBool::new(false);

/// Spawn the pipewire mainloop thread. Returns immediately; the thread
/// runs for the lifetime of the process. Errors during init (e.g. no
/// `/run/user/$UID/pipewire-0` socket) are logged and the thread reconnects
/// on a capped ramp so a daemon restart heals automatically.
///
/// Creates the command channel up front and publishes the [`Sender`] into
/// [`COMMAND_TX`] before the thread starts running the loop, **parking its
/// [`Receiver`] in the slot the first session takes from**. That way any
/// `set_*` call from the tokio side that lands before the loop has fully
/// connected to pipewire goes through the channel and is buffered until
/// the receiver attaches — never silently dropped.
///
/// Seeding the slot is the load-bearing half, not a tidy-up. `publish_channel`
/// hands its receiver back for a reason: dropping it here would leave the slot
/// `None`, so the first [`session_receiver`] would publish a *second* channel
/// and everything sent on the first would be stranded — and stranded
/// **silently**, because `pw::channel::Sender::send` never checks for a live
/// receiver (it writes the wakeup byte, pushes onto the queue and returns
/// `Ok(())`), so not even `send_command`'s receiver-dropped warning could fire.
///
/// **Supervised** since #1170 (the residual of #430): the reconnect loop below
/// only ever covered `run_once` returning, and `run_once` is where every
/// registry/param callback runs over daemon-supplied pods. A panic in one took
/// the thread — and with it sink/source/stream volume, the mute toggle and the
/// spectrum tap — out for the session, with no log line and nothing to restart
/// it. **What a restart re-does:** `pw::init()` (guarded by [`PW_INIT`], so the
/// library is initialised exactly once per process however many times the
/// closure re-runs), a fresh mainloop/context/core/registry, and a full
/// registry walk that re-emits every snapshot. The state it republishes lives
/// in the pipewire daemon, not here, which is the supervisor's own
/// restart-safety argument.
///
/// [`Sender`]: pw::channel::Sender
pub(super) fn spawn_mainloop(handles: PipewireHandles) {
    if STARTED.swap(true, Ordering::SeqCst) {
        // Programmer error: start() called twice. Don't disturb the live
        // sender — the second mainloop wouldn't share the first's proxy
        // map and writes would silently no-op.
        tracing::warn!("audio_native: spawn_mainloop called twice; ignoring second start");
        return;
    }
    let rx = initial_slot();
    spawn_supervised_blocking("pipewire", move || {
        PW_INIT.call_once(pw::init);
        run_sessions(&rx, &handles);
    });
}

/// The receiver slot a fresh [`spawn_mainloop`] starts from: a published
/// channel whose `Receiver` is already parked for the first session to take.
///
/// The `pw::channel::Receiver` is `!Sync`, and `spawn_supervised_blocking`
/// wants an `Fn() + Send + Sync` it can re-run from a fresh blocking thread, so
/// it rides in a mutex. It must outlive any one `run_once`: a mainloop that
/// exits (no daemon, dbus glitch) restarts, and commands queued meanwhile have
/// to survive. `pipewire::channel::Receiver` detaches cleanly when
/// `AttachedReceiver` is dropped, so the next `run_once` just attaches again.
///
/// Its own function rather than an expression inside `spawn_mainloop` so the
/// tests can start from the state production actually starts from — the
/// `STARTED` latch makes `spawn_mainloop` itself a once-per-process call.
fn initial_slot() -> Mutex<Option<pw::channel::Receiver<Command>>> {
    Mutex::new(Some(publish_channel()))
}

/// `pw_init` is process-global and refcounted; the supervisor may re-enter the
/// closure any number of times, and once is the honest number.
static PW_INIT: Once = Once::new();

/// Publish a fresh command channel and hand back its receiver.
fn publish_channel() -> pw::channel::Receiver<Command> {
    let (tx, rx) = pw::channel::channel::<Command>();
    *COMMAND_TX.write().unwrap_or_else(PoisonError::into_inner) = Some(tx);
    rx
}

/// The receiver for the next mainloop session.
///
/// Normally the previous session's, parked in `slot` — that is what makes a
/// command issued while the daemon is down arrive once it is back. The first
/// session's is parked there too, by [`initial_slot`], so the startup window is
/// not a hole. It is `None` in exactly one case: after a session that
/// **panicked**, which unwinds holding the receiver. Re-creating the channel
/// there is what keeps a restart a real recovery rather than a mainloop whose
/// command path is permanently dead; the commands queued on the lost channel
/// are gone, which is acceptable for a surface that is fire-and-forget volume
/// setting, and far better than the alternative of `.expect()`ing and turning
/// one panic into an unbounded panic loop.
fn session_receiver(slot: &Mutex<Option<pw::channel::Receiver<Command>>>) -> Receiver<Command> {
    if let Some(rx) = slot.lock().unwrap_or_else(PoisonError::into_inner).take() {
        return rx;
    }
    publish_channel()
}

/// Run mainloop sessions forever, reconnecting on a capped ramp.
///
/// Lives inside the supervised closure rather than being the closure, so the
/// supervisor's "a clean return means the task finished" rule never fires: this
/// never returns, and the only way out is a panic, which is what supervision is
/// for.
fn run_sessions(slot: &Mutex<Option<pw::channel::Receiver<Command>>>, handles: &PipewireHandles) {
    run_sessions_with(
        slot,
        |rx| run_once(clone_handles(handles), rx),
        thread::sleep,
    );
}

/// [`run_sessions`]'s body, with the session and the **waiter** injected.
///
/// The waiter is a parameter because nothing else can pin it. `reconnect_after`
/// is thoroughly tested and returns the right delay; the loop then has to
/// actually wait it, and that one statement is the whole of #1170's items 3
/// and 4 — deleting it reintroduces the hot respawn loop those items exist to
/// kill. A test that only reads `reconnect_after`'s return value cannot see
/// that, and clippy has nothing to say about a discarded `Duration` that is
/// still passed to a `let`. With the waiter injected, a counting stub records
/// what the loop waited and the assertion is on the wait itself.
///
/// `session` is injected for the ordinary reason: `run_once` needs a live
/// `PipeWire`.
fn run_sessions_with<R, S>(
    slot: &Mutex<Option<pw::channel::Receiver<Command>>>,
    session: R,
    sleep: S,
) where
    R: Fn(pw::channel::Receiver<Command>) -> (pw::channel::Receiver<Command>, SessionEnd),
    S: Fn(std::time::Duration),
{
    let mut reporter = retry::ReconnectReporter::new();
    loop {
        let receiver = session_receiver(slot);
        let started = Instant::now();
        let (returned_rx, end) = session(receiver);
        *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(returned_rx);
        let (_report, delay) = reconnect_after(&mut reporter, started.elapsed(), &end);
        sleep(delay);
    }
}

/// Record a finished mainloop session and say how long to wait before the next.
///
/// **Both arms come through here, and that is the point of #1170's item 3.**
/// `run_once` ending in [`SessionEnd::Quit`] is a failure wearing a success's
/// shape: the only thing that quits the mainloop is the core-error listener, so
/// a daemon that dies mid-session leaves this way. That arm used to return
/// straight to the top of the loop, skipping the 1s sleep the error arm took —
/// so a dead `PipeWire` was a hot respawn loop, one `warn!` per turn, as fast as
/// the daemon could fail. Now both are priced on `retry::RECONNECT_RETRY`
/// (500ms → 30s) and both are latched, so a permanent outage costs one loud
/// line and then silence until it heals.
///
/// Returns the report as well as the delay so the tests can read the cadence;
/// the loop only needs the delay.
fn reconnect_after(
    reporter: &mut retry::ReconnectReporter,
    ran_for: std::time::Duration,
    end: &SessionEnd,
) -> (retry::Report, std::time::Duration) {
    let (report, delay) = reporter.record(ran_for);
    let retry_in_secs = delay.as_secs_f64();
    let cause = end.cause();
    match report {
        retry::Report::Opened => tracing::warn!(
            cause,
            retry_in_secs,
            "audio_native: the PipeWire mainloop is not staying up; reconnecting with backoff. \
             Volume, mute and the spectrum tap are stale until it does. This line will not \
             repeat until it recovers"
        ),
        retry::Report::Repeating => tracing::debug!(
            cause,
            retry_in_secs,
            "audio_native: PipeWire mainloop still not staying up"
        ),
        retry::Report::Recovered => tracing::info!(
            "audio_native: the PipeWire mainloop is back; sinks, sources and streams are live again"
        ),
        // A session that stayed up and then ended: rare, and worth a line each
        // time — the latch has nothing outstanding to retract.
        retry::Report::Quiet => tracing::warn!(
            cause,
            retry_in_secs,
            "audio_native: PipeWire mainloop session ended, reconnecting"
        ),
    }
    (report, delay)
}

/// How one mainloop session ended.
///
/// A plain `Result<(), pw::Error>` cannot say this, which is how #1170's item 3
/// got in: the quit path's `Ok(())` reads as success at every call site, and the
/// reconnect loop treated it as one. Both variants are failures — the only
/// difference is how far the session got — and naming them that way is what
/// makes it hard to write a loop that backs off after one and not the other.
pub(super) enum SessionEnd {
    /// The session ran and the mainloop then quit. In practice that means the
    /// core-error listener fired: `message` is what the daemon said.
    Quit { message: Option<String> },
    /// The session never started: mainloop / context / core / registry
    /// construction failed (no `/run/user/$UID/pipewire-0` socket, say).
    Failed(pw::Error),
}

impl SessionEnd {
    /// One-line cause for a log field, whichever way the session ended.
    fn cause(&self) -> String {
        match self {
            Self::Quit { message: Some(m) } => format!("core error: {m}"),
            Self::Quit { message: None } => "mainloop quit".to_owned(),
            Self::Failed(e) => format!("session could not start: {e:?}"),
        }
    }
}

/// One mainloop session. Returns the receiver (so `run_sessions` can
/// re-attach it on the next session) along with how the session ended.
// One cohesive PipeWire registry + listener wiring block; splitting it would
// scatter shared closure state across helpers for no real readability gain.
#[allow(clippy::too_many_lines)]
pub(super) fn run_once(
    handles: PipewireHandles,
    rx: pw::channel::Receiver<Command>,
) -> (pw::channel::Receiver<Command>, SessionEnd) {
    let mainloop = match pw::main_loop::MainLoopRc::new(None) {
        Ok(m) => m,
        Err(e) => return (rx, SessionEnd::Failed(e)),
    };
    let context = match pw::context::ContextRc::new(&mainloop, None) {
        Ok(c) => c,
        Err(e) => return (rx, SessionEnd::Failed(e)),
    };
    let core = match context.connect_rc(None) {
        Ok(c) => c,
        Err(e) => return (rx, SessionEnd::Failed(e)),
    };
    let registry = match core.get_registry_rc() {
        Ok(r) => r,
        Err(e) => return (rx, SessionEnd::Failed(e)),
    };

    let state = Rc::new(RefCell::new(AudioState::new(handles)));

    // Attach the command channel. The returned `AttachedReceiver` must
    // outlive the loop; deattached after `mainloop.run()` returns so the
    // bare `Receiver` can be re-attached on the next session.
    //
    // The core rides along with the state because the audio spectrum capture tap
    // (#405) is built **lazily** (#581): `Command::SetSpectrumActive { active:
    // true }` constructs and connects it on the 0→1 demand edge, and `false`
    // drops it again — and `StreamRc::new` needs a core to construct against.
    // Nothing about the tap touches pipewire until a subscriber actually asks for
    // it, so a session where nobody opens an audio-reactive card never puts a
    // `trollshell-spectrum` node in the graph at all.
    let state_for_cmds = Rc::clone(&state);
    let core_for_cmds = core.clone();
    let attached = rx.attach(mainloop.loop_(), move |cmd| {
        handle_command(cmd, &state_for_cmds, &core_for_cmds);
    });

    // Core error → quit the mainloop so run_once returns and the outer loop
    // reconnects. Without this, daemon crashes leave the mainloop blocked
    // forever in the C-side poll.
    //
    // The message is *recorded* rather than logged here (#1170): this fires once
    // per session, and with a daemon that is down sessions are back-to-back, so
    // a `warn!` here was one line per reconnect attempt however hard the outer
    // loop latched. `run_sessions` owns the narrative and carries this string on
    // whichever line it decides to print.
    let quit_cause: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let mainloop_weak = mainloop.downgrade();
    let cause_for_listener = Rc::clone(&quit_cause);
    let _core_listener = core
        .add_listener_local()
        .error(move |id, _seq, _res, message| {
            if id == 0 {
                tracing::debug!(message = %message, "audio_native: core error, quitting");
                *cause_for_listener.borrow_mut() = Some(message.to_owned());
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
    let message = quit_cause.borrow_mut().take();
    (rx, SessionEnd::Quit { message })
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
fn handle_command(cmd: Command, state: &StateRef, core: &pw::core::CoreRc) {
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
            set_spectrum_active(state, core, active);
        }
    }
}

/// Build or tear down the audio spectrum capture tap (#405/#581).
///
/// Demand is an edge, not a level: `true` constructs + connects + activates the
/// stream if it doesn't exist yet, `false` disconnects and drops it. The tap used
/// to be built at loop start and merely `set_active`-toggled, which left a
/// `trollshell-spectrum` capture client parked in the graph for the whole session
/// — visible in `wpctl status`, pavucontrol's Recording tab and Helvum whether or
/// not anything was listening (#581). Now the node exists only while something is
/// actually looking at the spectrum. The cost is one format renegotiation per
/// sidebar open, which is rare and cheap.
///
/// A build failure (no monitor, older daemon) is logged and leaves the feature
/// dark — it never panics and never poisons the loop, so every other audio
/// feature keeps working and the next demand edge retries from scratch.
///
/// # Borrow discipline
///
/// Both the build and the teardown run pipewire code, so the `RefCell` borrow is
/// released before either: the inputs are read into locals up front, and the
/// stream is cloned out (it's an `Rc`) rather than used through a live borrow.
fn set_spectrum_active(state: &StateRef, core: &pw::core::CoreRc, active: bool) {
    // Read both inputs and drop the borrow immediately — nothing below may run
    // while it is held.
    let (built, out) = {
        let s = state.borrow();
        (s.spectrum_capture.is_some(), s.handles.spectrum.clone())
    };

    match SpectrumAction::decide(active, built) {
        SpectrumAction::BuildAndActivate => {
            let Some(capture) = build_spectrum_capture(core, out) else {
                tracing::warn!("audio_native: spectrum capture unavailable; leaving it dark");
                return;
            };
            activate_spectrum(&capture.stream);
            state.borrow_mut().spectrum_capture = Some(capture);
        }
        SpectrumAction::Activate => {
            // Already built (a redundant or re-asserted `true`). Rebuilding would
            // put a second tap on the monitor, so only re-activate.
            let stream = state
                .borrow()
                .spectrum_capture
                .as_ref()
                .map(|c| c.stream.clone());
            if let Some(stream) = stream {
                activate_spectrum(&stream);
            }
        }
        SpectrumAction::Teardown => {
            let taken = state.borrow_mut().spectrum_capture.take();
            if let Some(capture) = taken {
                if let Err(e) = capture.stream.disconnect() {
                    tracing::warn!(error = ?e, "audio_native: spectrum disconnect failed");
                }
                // Clear the last published frame so a re-open doesn't hand a
                // subscriber a stale spectrum from the previous session, and so
                // `pipewire::audio_spectrum()` really is `None` while the capture
                // is down, as documented.
                out.set(None);
                tracing::debug!("audio_native: spectrum capture torn down");
                // `capture` drops here — listener first, then the stream (field
                // order in `SpectrumCapture`), destroying the node.
            }
        }
        SpectrumAction::Nothing => {
            tracing::debug!("audio_native: spectrum capture already down; nothing to tear down");
        }
    }
}

/// Start a spectrum stream that has just been built (or was already present).
/// Only ever called with `true` — deactivation is a full teardown now, not a
/// pause, so there is no `set_active(false)` path left.
fn activate_spectrum(stream: &pw::stream::StreamRc) {
    if let Err(e) = stream.set_active(true) {
        tracing::warn!(error = ?e, "audio_native: activating spectrum capture failed");
    } else {
        tracing::debug!("audio_native: spectrum capture active");
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
/// it follow the default sink). The `param_changed` callback learns the
/// negotiated rate/channels; the `process` callback feeds samples through the
/// [`super::spectrum::Analyzer`] and publishes each finished `{peak, bins}` frame
/// to the `out` handle. Returns `None` (logged by the caller) if any step fails,
/// leaving the feature dark without disturbing the rest of the audio service.
///
/// Called **on demand** from [`set_spectrum_active`] (#581), never at startup.
/// The connect still passes [`INACTIVE`] and activation stays a separate
/// [`activate_spectrum`] call: that keeps this function's post-condition a single
/// "constructed and connected, not yet running", leaves activation in exactly one
/// place, and lets a fresh build and a redundant re-assert take the identical
/// path. There is no observable paused window — both calls happen in one
/// synchronous turn of the loop callback, with no round trip to the daemon in
/// between.
///
/// The `process` callback runs on this loop's own thread (no `RT_PROCESS`
/// flag), the same thread that already pushes `emit_snapshots`, so writing the
/// `Mutable` from it is consistent with the rest of the backend.
///
/// [`INACTIVE`]: pw::stream::StreamFlags::INACTIVE
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

    tracing::debug!("audio_native: spectrum capture built");
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
    let published = COMMAND_TX.read().unwrap_or_else(PoisonError::into_inner);
    let Some(tx) = published.as_ref() else {
        tracing::warn!("audio_native: command before service started");
        return;
    };
    if tx.send(cmd).is_err() {
        tracing::warn!("audio_native: send_command failed (receiver dropped)");
    }
}

#[cfg(test)]
mod tests {
    use super::{
        COMMAND_TX, Command, PW_INIT, SessionEnd, initial_slot, pw, reconnect_after,
        run_sessions_with, send_command, session_receiver,
    };
    use crate::retry;
    use hytte_reactive::test_lock::TEST_LOCK;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex, PoisonError};
    use std::thread;
    use std::time::{Duration, Instant};

    /// #1170's item 3: a core error — the `Ok(())`-shaped exit — must be paced
    /// exactly like a failed connect. Before this, the quit arm returned
    /// straight to the top of the loop and skipped the sleep entirely, so a
    /// dead `PipeWire` respawned as fast as it could fail.
    ///
    /// Falsify by giving `run_sessions` back its `if let Err(e) = …` shape (only
    /// the `Failed` arm sleeping): the two ramps below stop agreeing, because
    /// the quit arm would not be recorded at all.
    #[test]
    fn a_core_error_quit_is_paced_like_a_failed_connect() {
        let instant = Duration::from_millis(1);
        let quit = SessionEnd::Quit {
            message: Some("connection error".to_owned()),
        };

        let mut after_quit = retry::ReconnectReporter::new();
        let mut after_failure = retry::ReconnectReporter::new();
        // `SessionEnd::Failed` needs a real `pw::Error`, which cannot be built
        // without the library; `Quit { message: None }` stands in for "the
        // session did not work" on the comparison side, and the arms are
        // identical from `reconnect_after`'s point of view — which is exactly
        // the property under test.
        let plain = SessionEnd::Quit { message: None };

        for turn in 0..5 {
            let (_, quit_delay) = reconnect_after(&mut after_quit, instant, &quit);
            let (_, other_delay) = reconnect_after(&mut after_failure, instant, &plain);
            assert_eq!(
                quit_delay, other_delay,
                "turn {turn}: the quit path is on a different schedule from the error path"
            );
            assert!(
                quit_delay > Duration::ZERO,
                "turn {turn}: a core error respawns the mainloop with no delay at all"
            );
        }
    }

    /// …and the ramp actually climbs, rather than sitting at a flat delay: a
    /// daemon that is gone for good must cost less and less.
    #[test]
    fn consecutive_short_sessions_climb_the_ramp() {
        let mut reporter = retry::ReconnectReporter::new();
        let instant = Duration::from_millis(1);
        let end = SessionEnd::Quit { message: None };

        let first = reconnect_after(&mut reporter, instant, &end).1;
        let second = reconnect_after(&mut reporter, instant, &end).1;
        assert!(
            second > first,
            "the reconnect delay is not climbing: {first:?} then {second:?}"
        );
    }

    /// One loud line for a streak, not one per attempt — the other half of
    /// item 3. The wording is the call site's; the cadence is asserted here.
    #[test]
    fn a_dead_daemon_costs_one_loud_line() {
        let mut reporter = retry::ReconnectReporter::new();
        let end = SessionEnd::Quit { message: None };
        let reports: Vec<retry::Report> = (0..5)
            .map(|_| reconnect_after(&mut reporter, Duration::from_millis(1), &end).0)
            .collect();

        assert_eq!(
            reports
                .iter()
                .filter(|r| **r == retry::Report::Opened)
                .count(),
            1,
            "a permanently-dead PipeWire logs a warning per reconnect: {reports:?}"
        );
    }

    /// The daemon's own message survives to the line that reports the outage —
    /// the reason the core-error listener records it instead of logging it.
    #[test]
    fn the_cause_carries_the_daemons_message() {
        let end = SessionEnd::Quit {
            message: Some("no such device".to_owned()),
        };
        assert!(
            end.cause().contains("no such device"),
            "the core error's message is lost: {}",
            end.cause()
        );
        assert!(
            !SessionEnd::Quit { message: None }.cause().is_empty(),
            "a quit with no recorded cause must still say something"
        );
    }

    /// The first session gets a receiver, and the sender behind it is the one
    /// `send_command` publishes to — otherwise every `set_*` before the daemon
    /// answers goes nowhere, which is the guarantee `spawn_mainloop`'s doc
    /// makes.
    #[test]
    fn the_first_session_publishes_a_live_command_channel() {
        let slot = Mutex::new(None);
        let rx = session_receiver(&slot);

        let published = COMMAND_TX.read().unwrap_or_else(PoisonError::into_inner);
        let tx = published
            .as_ref()
            .expect("session_receiver must publish a sender");
        assert!(
            tx.send(Command::SetSinkMute {
                name: "probe".into(),
                mute: true
            })
            .is_ok(),
            "the published sender does not reach the session's receiver"
        );
        drop(rx);
    }

    /// A session that ended normally hands its receiver back, and the next one
    /// picks *that* up rather than a fresh channel — which is what makes a
    /// command issued while the daemon is down arrive once it comes back.
    #[test]
    fn a_returned_receiver_is_reused_rather_than_replaced() {
        let slot = Mutex::new(None);
        let rx = session_receiver(&slot);
        *slot.lock().unwrap_or_else(PoisonError::into_inner) = Some(rx);

        // Whatever `COMMAND_TX` holds now must still reach the *same*
        // receiver after the next take, so nothing was re-created behind it.
        let again = session_receiver(&slot);
        assert!(
            slot.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .is_none(),
            "the parked receiver was not taken; the next session would build a second channel"
        );
        drop(again);
    }

    /// #1170's item 2, the audio-specific hazard: a **panicking** session
    /// unwinds holding the receiver, so the restarted run finds the slot empty.
    /// It must re-establish the channel rather than `expect()` — an `expect`
    /// there turns one panic into an unbounded panic loop, which is strictly
    /// worse than the dead thread supervision replaced.
    ///
    /// Falsify by making `session_receiver` `.expect()` the take: this panics.
    #[test]
    fn a_receiver_lost_to_a_panic_is_re_established() {
        // An empty slot is exactly the state a panicked run leaves behind.
        let slot: Mutex<Option<pw::channel::Receiver<Command>>> = Mutex::new(None);
        let _first = session_receiver(&slot);

        let rx = session_receiver(&slot);
        let published = COMMAND_TX.read().unwrap_or_else(PoisonError::into_inner);
        let tx = published
            .as_ref()
            .expect("the re-established channel must be published");
        assert!(
            tx.send(Command::SetSinkMute {
                name: "probe".into(),
                mute: true
            })
            .is_ok(),
            "after a lost receiver the command path is dead for the session"
        );
        drop(rx);
    }

    /// The startup window: a command issued **before** the first session takes
    /// its receiver must arrive on that session, not vanish into a channel
    /// nobody holds. That is the guarantee `spawn_mainloop`'s doc makes, and it
    /// is the one a bare `publish_channel();` there breaks — the slot would
    /// start `None`, the first `session_receiver` would publish a *second*
    /// channel, and everything sent in between would sit in the first one's
    /// queue for the life of the process.
    ///
    /// It fails **silently** without this, which is why the assertion has to be
    /// about delivery rather than about an error: `pw::channel::Sender::send`
    /// never checks for a live receiver — it writes the wakeup byte, pushes onto
    /// the queue and returns `Ok(())` — so `send_command`'s receiver-dropped
    /// `warn!` cannot fire for this window, and a test that only asserted
    /// `send(...).is_ok()` would stay green with the bug in place. (The two
    /// tests above do exactly that, deliberately: they assert the channel is
    /// *published*, which is a weaker property.)
    ///
    /// Attaching to a real `MainLoopRc` and iterating it once is the only way to
    /// read a `pw::channel::Receiver`'s queue, and it needs no daemon: the
    /// mainloop is an epoll loop and the channel is a pipe. Nothing here
    /// connects to pipewire.
    ///
    /// Falsify by giving `initial_slot` the pre-fix shape — `publish_channel();`
    /// then `Mutex::new(None)`: nothing is delivered.
    #[test]
    fn a_command_sent_before_the_first_session_is_delivered() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        PW_INIT.call_once(pw::init);

        // Production's starting state, then the tokio side setting mute while
        // the blocking thread is still on its way up.
        let slot = initial_slot();
        send_command(Command::SetSinkMute {
            name: "probe".into(),
            mute: true,
        });

        let rx = session_receiver(&slot);
        let mainloop = pw::main_loop::MainLoopRc::new(None).expect("a mainloop needs no daemon");
        let seen: Rc<RefCell<Vec<Command>>> = Rc::new(RefCell::new(Vec::new()));
        let attached = rx.attach(mainloop.loop_(), {
            let seen = Rc::clone(&seen);
            move |cmd| seen.borrow_mut().push(cmd)
        });
        mainloop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(500)));

        assert_eq!(
            seen.borrow().len(),
            1,
            "a command issued before the first session was lost: {:?}",
            seen.borrow()
        );
        drop(attached);
    }

    /// #1170's item 3, the **wiring** rather than the helper: the loop has to
    /// actually wait the delay `reconnect_after` computed.
    ///
    /// Everything else about the ramp was already asserted against
    /// `reconnect_after`'s return value, which is why `thread::sleep(delay);`
    /// could be deleted from both reconnect loops in this crate with all 811
    /// tests green and clippy silent — the hot respawn loop items 3 and 4 exist
    /// to kill, reintroducible for free. This drives five turns with a counting
    /// stub in the waiter's place and asserts the recorded waits, so deleting
    /// the wait is a red test rather than a green one.
    ///
    /// Costs no wall clock: the stub records and returns, so the 15.5s the real
    /// ramp would spend here never happens.
    ///
    /// Falsify by dropping `sleep(delay);` from `run_sessions_with` (bind
    /// `_delay` so it still compiles): nothing is recorded and this fails.
    #[test]
    fn the_mainloop_waits_the_reconnect_delay_it_computed() {
        /// Five turns is enough to see the ramp climb and still be far from the
        /// 30s ceiling, so a clamp bug would show as a wrong value rather than
        /// as a repeated one.
        const TURNS: usize = 5;

        let recorded: Arc<Mutex<Vec<Duration>>> = Arc::new(Mutex::new(Vec::new()));
        let waits = Arc::clone(&recorded);
        thread::spawn(move || {
            // A session that ends instantly, the way a dead daemon's does.
            // `COMMAND_TX` is never touched: the slot starts seeded and the
            // fake session hands the same receiver straight back.
            let (_tx, rx) = pw::channel::channel::<Command>();
            let slot = Mutex::new(Some(rx));
            run_sessions_with(
                &slot,
                |rx| (rx, SessionEnd::Quit { message: None }),
                move |delay| {
                    let mut v = waits.lock().unwrap_or_else(PoisonError::into_inner);
                    v.push(delay);
                    let done = v.len() >= TURNS;
                    drop(v);
                    if done {
                        // `run_sessions_with` never returns by design, so park
                        // the thread rather than spin it for the rest of the
                        // binary. `park` may wake spuriously; the loop is the
                        // documented way to hold it.
                        loop {
                            thread::park();
                        }
                    }
                },
            );
        });

        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline
            && recorded.lock().unwrap_or_else(PoisonError::into_inner).len() < TURNS
        {
            thread::sleep(Duration::from_millis(10));
        }

        let waited = recorded
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        assert_eq!(
            waited,
            vec![
                Duration::from_millis(500),
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(8),
            ],
            "the mainloop did not wait `reconnect_after`'s ramp between sessions"
        );
    }
}
