//! Internal data types shared across the `audio_native` submodules.
//!
//! All items here are `pub(super)` — only visible inside `audio_native`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use pipewire as pw;

use super::super::pipewire::{PipewireHandles, PlaybackStream, RecordStream, Sink, Source, Volume};

/// Coarse classification of a `PipeWire` Node by its `media.class` property.
///
/// `PipeWire` nodes can be many things (cameras, MIDI, ALSA cards, virtual
/// devices, application streams). For volume-control purposes only four
/// kinds matter; everything else is filtered out at registry-walk time.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AudioRole {
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
    pub(super) fn from_media_class(class: &str) -> Option<Self> {
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
pub(super) struct NodeEntry {
    pub(super) role: AudioRole,
    /// `node.name` — the canonical id pactl uses and the one that survives
    /// across reconnects. Used to mark `is_default` against the Metadata
    /// `default.audio.sink` value, and as the argument to `set_default_sink`.
    pub(super) name: String,
    /// `node.description` — human-readable; falls back to `name`.
    pub(super) description: String,
    /// For streams: best-effort app name. `None` for Audio/Sink, Audio/Source.
    pub(super) app_name: Option<String>,
    /// Per-channel linear gain, populated from `SPA_PROP_channelVolumes`.
    /// Empty until the first Props event arrives. `PipeWire`'s channelVolumes
    /// is already in linear-gain space (1.0 = 100%), matching the `Sink`
    /// public API field — no cube transform needed.
    pub(super) channel_volumes: Vec<f32>,
    /// `SPA_PROP_mute`. Defaults to `false` until the first Props event.
    pub(super) mute: bool,
}

/// Per-node proxy + listener pair. Both must stay alive until the node
/// global is removed, otherwise the C-side closure is dropped and the
/// param events stop arriving.
pub(super) struct NodeProxy {
    #[allow(dead_code)] // kept alive solely for its Drop side-effects
    pub(super) proxy: pw::node::Node,
    #[allow(dead_code)]
    pub(super) listener: pw::node::NodeListener,
}

/// Proxy + listener for the single `default` Metadata global. Pipewire
/// emits multiple Metadata instances (e.g. per-device route metadata); we
/// bind only the one named `default`. Like `NodeProxy`, both must stay
/// alive together — dropping either ends property events.
pub(super) struct MetadataProxy {
    #[allow(dead_code)]
    pub(super) proxy: pw::metadata::Metadata,
    #[allow(dead_code)]
    pub(super) listener: pw::metadata::MetadataListener,
}

/// One edge of the `PipeWire` graph — used to resolve stream→sink (or
/// stream→source) routing without binding the Link proxy. The values are
/// the node ids on each side; the Link's own id is the [`AudioState::links`]
/// map key.
#[derive(Clone, Copy, Debug)]
pub(super) struct LinkEdge {
    pub(super) output_node: u32,
    pub(super) input_node: u32,
}

/// The audio spectrum capture tap (#405): a `PipeWire` input stream connected to
/// the default sink's monitor plus its listener. Both must stay alive together —
/// dropping either stops capture. Held `Option`ally in [`AudioState`] because
/// the stream is created after the core connects, and is toggled active/inactive
/// via [`Command::SetSpectrumActive`] so an idle desktop (no subscriber) does no
/// analysis work.
pub(super) struct SpectrumCapture {
    // Field order is the drop order: the listener must drop **before** the
    // stream, because unregistering a listener (`spa_hook_remove`) walks the
    // stream's still-live listener list. Dropping the stream first would free
    // that list out from under it.
    #[allow(dead_code)] // kept alive solely so its callbacks keep firing
    pub(super) listener: pw::stream::StreamListener<super::spectrum::SpectrumUserData>,
    pub(super) stream: pw::stream::StreamRc,
}

/// State owned by the pipewire-loop thread. Wrapped in `Rc<RefCell<_>>` so
/// registry-event callbacks (each its own `FnMut`-bound closure) share access.
pub(super) struct AudioState {
    /// Node identity + volume cache, keyed by `PipeWire` global id.
    pub(super) nodes: HashMap<u32, NodeEntry>,
    /// Live proxies for every bound Node. Dropping the entry destroys the
    /// proxy and detaches its listener, ending param events.
    pub(super) proxies: HashMap<u32, NodeProxy>,
    /// Live proxy for the `default` Metadata global (one per session).
    /// Populated lazily when the registry walker first sees a Metadata
    /// global with `metadata.name = "default"`.
    pub(super) metadata_default: Option<MetadataProxy>,
    /// `default.audio.sink` value — the `node.name` of the current default
    /// sink. Compared against each Sink's name in `emit_snapshots` to
    /// flip `is_default`. `None` until the first property event fires.
    pub(super) default_sink_name: Option<String>,
    /// Same as `default_sink_name` for the default source.
    pub(super) default_source_name: Option<String>,
    /// Graph-edge cache, keyed by Link global id. Built from registry
    /// add events (the props dict carries `link.{output,input}.node`).
    /// Stream→sink routing is derived from this in `emit_snapshots` by
    /// finding the link whose `output_node` matches the stream id.
    pub(super) links: HashMap<u32, LinkEdge>,
    /// The audio spectrum capture tap (#405), created once the core connects.
    /// `None` until built (or if creation failed — e.g. no monitor available).
    pub(super) spectrum_capture: Option<SpectrumCapture>,
    /// Output Mutables — fresh snapshots are pushed here after every state
    /// change. Cloning a `Mutable` clones the `Arc` inside, so this struct
    /// shares ownership with whatever the `Service::start` caller holds.
    pub(super) handles: PipewireHandles,
    /// Last snapshot pushed to each Mutable, used to skip redundant
    /// `set()` calls that would tear down subscribers' diff state for a
    /// no-op. The Mutables already dedup by `PartialEq`, but doing the
    /// comparison here saves the Vec clone on a hot path.
    pub(super) last_sink_volume: Volume,
    pub(super) last_sinks: Vec<Sink>,
    pub(super) last_sources: Vec<Source>,
    pub(super) last_streams: Vec<PlaybackStream>,
    pub(super) last_record_streams: Vec<RecordStream>,
}

impl AudioState {
    pub(super) fn new(handles: PipewireHandles) -> Self {
        Self {
            nodes: HashMap::new(),
            proxies: HashMap::new(),
            metadata_default: None,
            default_sink_name: None,
            default_source_name: None,
            links: HashMap::new(),
            spectrum_capture: None,
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
pub(super) enum Command {
    /// Set channelVolumes on a sink, identified by `node.name`. The new
    /// linear gain is replicated across every channel currently in the
    /// cache so a stereo sink stays stereo. If the cache is empty (no
    /// Props event received yet) the command is dropped — we'd otherwise
    /// publish a mono channelVolumes array and clobber the sink's layout.
    SetSinkVolume {
        name: String,
        linear: f64,
    },
    /// Set channelVolumes on a source (microphone, line-in).
    SetSourceVolume {
        name: String,
        linear: f64,
    },
    /// Set channelVolumes on a playback stream, identified by pipewire
    /// global id (streams have no stable `node.name`).
    SetStreamVolume {
        id: u32,
        linear: f64,
    },
    /// Set `SPA_PROP_mute` on a sink.
    SetSinkMute {
        name: String,
        mute: bool,
    },
    SetSourceMute {
        name: String,
        mute: bool,
    },
    SetStreamMute {
        id: u32,
        mute: bool,
    },
    /// Write `default.audio.sink` on the `default` Metadata. Phase 4 wires
    /// this — for Phase 3 the dispatcher logs and drops the command.
    SetDefaultSink {
        name: String,
    },
    SetDefaultSource {
        name: String,
    },
    /// Activate or deactivate the audio spectrum capture tap (#405). The plugin
    /// host toggles this so the monitor is only tapped while at least one plugin
    /// subscribes `StateKey::AudioSpectrum` — an idle desktop does no analysis.
    /// A no-op if the capture stream failed to build.
    SetSpectrumActive {
        active: bool,
    },
}

/// Clone a `PipewireHandles` for cross-thread sharing. Each `Mutable` is
/// internally an `Arc` so clones see the same backing storage.
pub(super) fn clone_handles(h: &PipewireHandles) -> PipewireHandles {
    PipewireHandles {
        sink: h.sink.clone(),
        sinks: h.sinks.clone(),
        sources: h.sources.clone(),
        streams: h.streams.clone(),
        record_streams: h.record_streams.clone(),
        spectrum: h.spectrum.clone(),
    }
}

/// Shared state reference type used throughout the module.
pub(super) type StateRef = Rc<RefCell<AudioState>>;
