//! Audio device + stream state from `pactl`.
//!
//! State is fetched once at startup and then refetched on every relevant
//! event from `pactl subscribe` (sink, source, sink-input, source-output,
//! server). Emissions are deduped against the previously-emitted snapshot
//! so consumers don't tear down and rebuild on no-op updates. The default
//! sink's `Volume` is derived from the same fetched state, so there's no
//! separate poll path for the bar chip.
//!
//! v0.8+ should consider a real `pipewire-rs` registry subscription if we
//! ever want to drop the pactl shell-out entirely.

use futures_signals::signal::{Mutable, Signal};
use hytte_reactive::{registry, Service};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::Duration;

pub struct PipewireService;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Volume {
    /// Linear volume, `0.0..=1.0` (may exceed 1.0 if user boosts above
    /// 100%). Untouched on parse failure.
    pub linear: f64,
    pub muted: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Sink {
    pub id: u32,
    pub name: String,
    pub description: String,
    pub volume: f64,
    pub muted: bool,
    pub is_default: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Source {
    pub id: u32,
    pub name: String,
    pub description: String,
    pub volume: f64,
    pub muted: bool,
    pub is_default: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PlaybackStream {
    pub id: u32,
    pub app_name: String,
    pub sink_id: u32,
    pub volume: f64,
    pub muted: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RecordStream {
    pub id: u32,
    pub app_name: String,
    pub source_id: u32,
    pub volume: f64,
    pub muted: bool,
}

#[doc(hidden)]
pub struct PipewireHandles {
    pub(crate) sink: Mutable<Volume>,
    pub(crate) sinks: Mutable<Vec<Sink>>,
    pub(crate) sources: Mutable<Vec<Source>>,
    pub(crate) streams: Mutable<Vec<PlaybackStream>>,
    pub(crate) record_streams: Mutable<Vec<RecordStream>>,
}

impl Default for PipewireHandles {
    fn default() -> Self {
        Self {
            sink: Mutable::new(Volume::default()),
            sinks: Mutable::new(Vec::new()),
            sources: Mutable::new(Vec::new()),
            streams: Mutable::new(Vec::new()),
            record_streams: Mutable::new(Vec::new()),
        }
    }
}

impl Service for PipewireService {
    type Handles = PipewireHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = PipewireHandles::default();

        // Single event-driven task: do an initial full-state read to seed the
        // signals, then attach to `pactl subscribe` and refetch on each event.
        // Replaces the old 250 ms wpctl poll + 1 Hz pactl poll. The bar
        // widget's `default_sink()` Volume is derived from the default sink
        // in the same fetched state — no separate polling path.
        //
        // Run blocking because: (a) tokio is built with `rt` only here, no
        // process/io-util features; (b) `pactl list ...` shells out anyway
        // and shouldn't share a tokio runtime worker.
        let sink_writer = handles.sink.clone();
        let sinks_writer = handles.sinks.clone();
        let sources_writer = handles.sources.clone();
        let streams_writer = handles.streams.clone();
        let record_writer = handles.record_streams.clone();
        rt.spawn_blocking(move || {
            let mut last_default = Volume::default();
            let mut last_sinks: Vec<Sink> = Vec::new();
            let mut last_sources: Vec<Source> = Vec::new();
            let mut last_streams: Vec<PlaybackStream> = Vec::new();
            let mut last_records: Vec<RecordStream> = Vec::new();

            let mut emit = |state: FullState| {
                let default_v = state
                    .sinks
                    .iter()
                    .find(|s| s.is_default)
                    .map_or(Volume::default(), |s| Volume {
                        linear: s.volume,
                        muted: s.muted,
                    });
                if default_v != last_default {
                    last_default = default_v;
                    sink_writer.set(default_v);
                }
                if state.sinks != last_sinks {
                    last_sinks.clone_from(&state.sinks);
                    sinks_writer.set(state.sinks);
                }
                if state.sources != last_sources {
                    last_sources.clone_from(&state.sources);
                    sources_writer.set(state.sources);
                }
                if state.streams != last_streams {
                    last_streams.clone_from(&state.streams);
                    streams_writer.set(state.streams);
                }
                if state.record_streams != last_records {
                    last_records.clone_from(&state.record_streams);
                    record_writer.set(state.record_streams);
                }
            };

            // Initial seed.
            if let Some(state) = read_full_state() {
                emit(state);
            }

            // Subscribe loop with restart-on-death backoff.
            loop {
                let Ok(mut child) = Command::new("pactl")
                    .arg("subscribe")
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                else {
                    std::thread::sleep(Duration::from_secs(2));
                    continue;
                };
                let Some(stdout) = child.stdout.take() else {
                    let _ = child.kill();
                    std::thread::sleep(Duration::from_secs(2));
                    continue;
                };

                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else { break };
                    if !is_relevant_event(&line) {
                        continue;
                    }
                    if let Some(state) = read_full_state() {
                        emit(state);
                    }
                }

                // pactl subscribe died — clean up and retry shortly.
                let _ = child.kill();
                let _ = child.wait();
                std::thread::sleep(Duration::from_secs(1));
            }
        });

        handles
    }
}

/// True for `pactl subscribe` event lines that affect sinks, sources,
/// playback streams, record streams, or default-device assignment. Skips
/// noisy categories like `client`, `card`, `module`.
fn is_relevant_event(line: &str) -> bool {
    let Some(rest) = line.split(" on ").nth(1) else {
        return false;
    };
    let cat = rest.split('#').next().unwrap_or("").trim();
    matches!(
        cat,
        "sink" | "source" | "sink-input" | "source-output" | "server"
    )
}

// ── pactl parsing ─────────────────────────────────────────────────────────────

struct FullState {
    sinks: Vec<Sink>,
    sources: Vec<Source>,
    streams: Vec<PlaybackStream>,
    record_streams: Vec<RecordStream>,
}

fn run_cmd(args: &[&str]) -> Option<String> {
    let out = Command::new(args[0]).args(&args[1..]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// Split long-form `pactl list ...` output into per-item `HashMap<String,
/// String>`. Each item block starts with a line matching `block_prefix` (e.g.
/// `"Sink #"` or `"Sink Input #"`). Returns one map per block; map keys are
/// trimmed field names, values are trimmed field values.
///
/// Properties (indented with two tabs) are stored with their quoted values
/// stripped, e.g. key `"application.name"` → value `"Firefox"`.
fn parse_pactl_blocks(output: &str, block_prefix: &str) -> Vec<HashMap<String, String>> {
    let mut blocks: Vec<HashMap<String, String>> = Vec::new();
    let mut current: Option<HashMap<String, String>> = None;
    let mut in_properties = false;

    for line in output.lines() {
        // Block header: "Sink #42", "Sink Input #51", etc.
        if line.starts_with(block_prefix) {
            if let Some(prev) = current.take() {
                blocks.push(prev);
            }
            current = Some(HashMap::new());
            in_properties = false;
            continue;
        }

        let Some(map) = current.as_mut() else {
            continue;
        };

        // Detect the "Properties:" section boundary.
        if line.trim() == "Properties:" {
            in_properties = true;
            continue;
        }

        if in_properties {
            // Property lines: "\t\tapplication.name = \"Firefox\""
            let trimmed = line.trim();
            if let Some((k, v)) = trimmed.split_once('=') {
                let key = k.trim().to_string();
                let val = v.trim().trim_matches('"').to_string();
                map.entry(key).or_insert(val);
            }
        } else {
            // Regular field lines: "\tDescription: Built-in Audio Analog Stereo"
            let trimmed = line.trim();
            if let Some((k, v)) = trimmed.split_once(':') {
                let key = k.trim().to_string();
                let val = v.trim().to_string();
                map.entry(key).or_insert(val);
            }
        }
    }

    if let Some(last) = current.take() {
        blocks.push(last);
    }

    blocks
}

fn read_full_state() -> Option<FullState> {
    let info_out = run_cmd(&["pactl", "info"])?;
    let default_sink_name = parse_pactl_info_field(&info_out, "Default Sink");
    let default_source_name = parse_pactl_info_field(&info_out, "Default Source");

    // ── Sinks ────────────────────────────────────────────────────────────────
    let sinks_short = run_cmd(&["pactl", "list", "sinks", "short"])?;
    let sinks_long_out = run_cmd(&["pactl", "list", "sinks"]).unwrap_or_default();
    let sink_info = parse_device_info_from_long(&sinks_long_out, "Sink #");

    let mut sinks = Vec::new();
    for line in sinks_short.lines() {
        let parts: Vec<&str> = line.splitn(5, '\t').collect();
        if parts.len() < 2 {
            continue;
        }
        let id: u32 = parts[0].trim().parse().ok()?;
        let name = parts[1].trim().to_string();
        let info = sink_info.get(&name);
        let description = info.map_or_else(|| name.clone(), |i| i.description.clone());
        let volume = info.map_or(0.0, |i| i.volume);
        let muted = info.is_some_and(|i| i.muted);
        let is_default = default_sink_name.as_deref() == Some(name.as_str());
        sinks.push(Sink {
            id,
            name,
            description,
            volume,
            muted,
            is_default,
        });
    }

    // ── Sources (filter .monitor) ─────────────────────────────────────────────
    let sources_short = run_cmd(&["pactl", "list", "sources", "short"])?;
    let sources_long_out = run_cmd(&["pactl", "list", "sources"]).unwrap_or_default();
    let source_info = parse_device_info_from_long(&sources_long_out, "Source #");

    let mut sources = Vec::new();
    for line in sources_short.lines() {
        let parts: Vec<&str> = line.splitn(5, '\t').collect();
        if parts.len() < 2 {
            continue;
        }
        let name = parts[1].trim().to_string();
        // Filter out monitor sources (loopback from sinks).
        if name.ends_with(".monitor") {
            continue;
        }
        let id: u32 = parts[0].trim().parse().ok()?;
        let info = source_info.get(&name);
        let description = info.map_or_else(|| name.clone(), |i| i.description.clone());
        let volume = info.map_or(0.0, |i| i.volume);
        let muted = info.is_some_and(|i| i.muted);
        let is_default = default_source_name.as_deref() == Some(name.as_str());
        sources.push(Source {
            id,
            name,
            description,
            volume,
            muted,
            is_default,
        });
    }

    // ── Sink inputs (playback streams) ────────────────────────────────────────
    let streams = parse_playback_streams();

    // ── Source outputs (record streams) ───────────────────────────────────────
    let record_streams = parse_record_streams();

    Some(FullState {
        sinks,
        sources,
        streams,
        record_streams,
    })
}

fn parse_pactl_info_field(output: &str, field: &str) -> Option<String> {
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix(field)
            && let Some(val) = rest.strip_prefix(':')
        {
            return Some(val.trim().to_string());
        }
    }
    None
}

struct DeviceInfo {
    description: String,
    volume: f64,
    muted: bool,
}

/// Build a map of `name → DeviceInfo` from long-form pactl sink/source output.
/// Reads `Description`, `Volume`, and `Mute` directly so we don't need a
/// per-device wpctl shell-out and don't depend on pactl/wpctl id namespaces
/// agreeing.
fn parse_device_info_from_long(output: &str, block_prefix: &str) -> HashMap<String, DeviceInfo> {
    let mut map = HashMap::new();
    for block in parse_pactl_blocks(output, block_prefix) {
        let Some(name) = block.get("Name") else {
            continue;
        };
        let description = block
            .get("Description")
            .cloned()
            .unwrap_or_else(|| name.clone());
        let volume = block
            .get("Volume")
            .and_then(|s| parse_pactl_volume(s))
            .unwrap_or(0.0);
        let muted = block.get("Mute").is_some_and(|s| parse_pactl_mute(s));
        map.insert(
            name.clone(),
            DeviceInfo {
                description,
                volume,
                muted,
            },
        );
    }
    map
}

fn parse_playback_streams() -> Vec<PlaybackStream> {
    let Some(long_out) = run_cmd(&["pactl", "list", "sink-inputs"]) else {
        return Vec::new();
    };
    parse_sink_input_blocks_with_ids(&long_out)
}

fn parse_record_streams() -> Vec<RecordStream> {
    let Some(long_out) = run_cmd(&["pactl", "list", "source-outputs"]) else {
        return Vec::new();
    };
    parse_source_output_blocks_with_ids(&long_out)
}

fn parse_source_output_blocks_with_ids(output: &str) -> Vec<RecordStream> {
    let mut streams = Vec::new();
    let mut current_id: Option<u32> = None;
    let mut current_map: HashMap<String, String> = HashMap::new();
    let mut in_properties = false;

    for line in output.lines() {
        if line.starts_with("Source Output #") {
            if let Some(id) = current_id.take()
                && let Some(stream) = build_record_stream(id, &current_map)
            {
                streams.push(stream);
            }
            current_map.clear();
            in_properties = false;

            if let Some(id_str) = line.strip_prefix("Source Output #") {
                current_id = id_str.trim().parse().ok();
            }
            continue;
        }

        if current_id.is_none() {
            continue;
        }

        if line.trim() == "Properties:" {
            in_properties = true;
            continue;
        }

        if in_properties {
            let trimmed = line.trim();
            if let Some((k, v)) = trimmed.split_once('=') {
                let key = k.trim().to_string();
                let val = v.trim().trim_matches('"').to_string();
                current_map.entry(key).or_insert(val);
            }
        } else {
            let trimmed = line.trim();
            if let Some((k, v)) = trimmed.split_once(':') {
                let key = k.trim().to_string();
                let val = v.trim().to_string();
                current_map.entry(key).or_insert(val);
            }
        }
    }

    if let Some(id) = current_id.take()
        && let Some(stream) = build_record_stream(id, &current_map)
    {
        streams.push(stream);
    }

    streams
}

fn build_record_stream(id: u32, map: &HashMap<String, String>) -> Option<RecordStream> {
    let source_id: u32 = map.get("Source")?.trim().parse().ok()?;
    // Filter out PulseAudio's own monitoring/peek streams that GNOME-style
    // indicators ignore. These typically have media.class = Stream/Input/Audio
    // and media.role = "peek", or come from pavucontrol/wireplumber itself.
    if map.get("media.role").map(String::as_str) == Some("peek") {
        return None;
    }
    let app_name = pick_app_name(map, id);
    let volume = map
        .get("Volume")
        .and_then(|s| parse_pactl_volume(s))
        .unwrap_or(0.0);
    let muted = map.get("Mute").is_some_and(|s| parse_pactl_mute(s));
    Some(RecordStream {
        id,
        app_name,
        source_id,
        volume,
        muted,
    })
}

fn parse_sink_input_blocks_with_ids(output: &str) -> Vec<PlaybackStream> {
    let mut streams = Vec::new();
    let mut current_id: Option<u32> = None;
    let mut current_map: HashMap<String, String> = HashMap::new();
    let mut in_properties = false;

    for line in output.lines() {
        if line.starts_with("Sink Input #") {
            // Flush previous block.
            if let Some(id) = current_id.take()
                && let Some(stream) = build_playback_stream(id, &current_map)
            {
                streams.push(stream);
            }
            current_map.clear();
            in_properties = false;

            // Parse the id from the header.
            if let Some(id_str) = line.strip_prefix("Sink Input #") {
                current_id = id_str.trim().parse().ok();
            }
            continue;
        }

        if current_id.is_none() {
            continue;
        }

        if line.trim() == "Properties:" {
            in_properties = true;
            continue;
        }

        if in_properties {
            let trimmed = line.trim();
            if let Some((k, v)) = trimmed.split_once('=') {
                let key = k.trim().to_string();
                let val = v.trim().trim_matches('"').to_string();
                current_map.entry(key).or_insert(val);
            }
        } else {
            let trimmed = line.trim();
            if let Some((k, v)) = trimmed.split_once(':') {
                let key = k.trim().to_string();
                let val = v.trim().to_string();
                current_map.entry(key).or_insert(val);
            }
        }
    }

    // Final block.
    if let Some(id) = current_id.take()
        && let Some(stream) = build_playback_stream(id, &current_map)
    {
        streams.push(stream);
    }

    streams
}

fn build_playback_stream(id: u32, map: &HashMap<String, String>) -> Option<PlaybackStream> {
    let sink_id: u32 = map.get("Sink")?.trim().parse().ok()?;
    let app_name = pick_app_name(map, id);
    let volume = map
        .get("Volume")
        .and_then(|s| parse_pactl_volume(s))
        .unwrap_or(0.0);
    let muted = map.get("Mute").is_some_and(|s| parse_pactl_mute(s));
    Some(PlaybackStream {
        id,
        app_name,
        sink_id,
        volume,
        muted,
    })
}

/// Pick a user-facing app name for a stream from its parsed pactl property
/// map. Some apps (notably Spotify) publish only `node.name` / `media.name`
/// over the pipewire-pulse compat layer, so the chain falls back through
/// several candidates before resorting to `Stream {id}`.
///
/// Empty values are skipped, and so are known-generic placeholders like
/// `audio-src` or `Loopback` that pipewire assigns to anonymous link-nodes
/// — those would otherwise shadow a useful `media.name` further down.
fn pick_app_name(map: &HashMap<String, String>, id: u32) -> String {
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
        if let Some(v) = map.get(*key) {
            let t = v.trim();
            if !t.is_empty() && !GENERIC.iter().any(|g| g.eq_ignore_ascii_case(t)) {
                return t.to_string();
            }
        }
    }
    format!("Stream {id}")
}

/// Parse a pactl `Volume:` field value, e.g.
/// `front-left: 32768 /  50% / -6.02 dB,   front-right: 32768 /  50% / -6.02 dB`
/// → 0.50 (first channel's percentage, as linear 0.0..=1.0+).
fn parse_pactl_volume(s: &str) -> Option<f64> {
    let pct_str = s.split('%').next()?.rsplit_once(' ').map(|(_, n)| n)?;
    let pct: f64 = pct_str.trim().parse().ok()?;
    Some(pct / 100.0)
}

fn parse_pactl_mute(s: &str) -> bool {
    s.trim().eq_ignore_ascii_case("yes")
}

// ── Commands (fire-and-forget) ─────────────────────────────────────────────────

fn spawn_cmd(mut cmd: Command) {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    hytte_reactive::runtime::handle().spawn_blocking(move || {
        let _ = cmd.status();
    });
}

pub fn set_default_sink(name: &str) {
    let name = name.to_string();
    spawn_cmd({
        let mut c = Command::new("pactl");
        c.args(["set-default-sink", &name]);
        c
    });
}

pub fn set_default_source(name: &str) {
    let name = name.to_string();
    spawn_cmd({
        let mut c = Command::new("pactl");
        c.args(["set-default-source", &name]);
        c
    });
}

/// Format a linear (0.0..=1.0+) volume as a pactl percentage argument,
/// clamped to 0..=200% to stay inside u32 range and avoid sending nonsense
/// values if a slider misbehaves.
fn linear_to_pct(linear: f64) -> String {
    let pct = (linear * 100.0).round().clamp(0.0, 200.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let pct = pct as u32;
    format!("{pct}%")
}

pub fn set_sink_volume(name: &str, linear: f64) {
    let pct = linear_to_pct(linear);
    let name = name.to_string();
    spawn_cmd({
        let mut c = Command::new("pactl");
        c.args(["set-sink-volume", &name, &pct]);
        c
    });
}

pub fn set_source_volume(name: &str, linear: f64) {
    let pct = linear_to_pct(linear);
    let name = name.to_string();
    spawn_cmd({
        let mut c = Command::new("pactl");
        c.args(["set-source-volume", &name, &pct]);
        c
    });
}

pub fn set_stream_volume(id: u32, linear: f64) {
    // pactl indexes sink-inputs with its own pulse-compat numbering, which is
    // not always the same as a pipewire object id — so wpctl can fail to find
    // these. Use pactl end-to-end here.
    let pct = linear_to_pct(linear);
    let id_str = id.to_string();
    spawn_cmd({
        let mut c = Command::new("pactl");
        c.args(["set-sink-input-volume", &id_str, &pct]);
        c
    });
}

pub fn set_sink_mute(name: &str, mute: bool) {
    let m = if mute { "1" } else { "0" };
    let name = name.to_string();
    spawn_cmd({
        let mut c = Command::new("pactl");
        c.args(["set-sink-mute", &name, m]);
        c
    });
}

pub fn set_source_mute(name: &str, mute: bool) {
    let m = if mute { "1" } else { "0" };
    let name = name.to_string();
    spawn_cmd({
        let mut c = Command::new("pactl");
        c.args(["set-source-mute", &name, m]);
        c
    });
}

pub fn set_stream_mute(id: u32, mute: bool) {
    let m = if mute { "1" } else { "0" };
    let id_str = id.to_string();
    spawn_cmd({
        let mut c = Command::new("pactl");
        c.args(["set-sink-input-mute", &id_str, m]);
        c
    });
}

// ── Public API ────────────────────────────────────────────────────────────────

#[must_use]
pub fn service() -> PipewireService {
    PipewireService
}

pub fn default_sink() -> impl Signal<Item = Volume> {
    registry::with(|r| {
        r.get::<PipewireHandles>()
            .expect("pipewire::service() not registered")
            .sink
            .signal_cloned()
    })
}

pub fn sinks() -> impl Signal<Item = Vec<Sink>> {
    registry::with(|r| {
        r.get::<PipewireHandles>()
            .expect("pipewire::service() not registered")
            .sinks
            .signal_cloned()
    })
}

pub fn sources() -> impl Signal<Item = Vec<Source>> {
    registry::with(|r| {
        r.get::<PipewireHandles>()
            .expect("pipewire::service() not registered")
            .sources
            .signal_cloned()
    })
}

pub fn playback_streams() -> impl Signal<Item = Vec<PlaybackStream>> {
    registry::with(|r| {
        r.get::<PipewireHandles>()
            .expect("pipewire::service() not registered")
            .streams
            .signal_cloned()
    })
}

pub fn record_streams() -> impl Signal<Item = Vec<RecordStream>> {
    registry::with(|r| {
        r.get::<PipewireHandles>()
            .expect("pipewire::service() not registered")
            .record_streams
            .signal_cloned()
    })
}

/// Set the default sink's linear volume (0.0..=1.0+). Fire-and-forget;
/// runs `wpctl set-volume` on the tokio runtime so the GTK main thread
/// stays responsive during continuous slider drags.
pub fn set_volume(linear: f64) {
    hytte_reactive::runtime::handle().spawn_blocking(move || {
        let arg = format!("{linear:.4}");
        let _ = Command::new("wpctl")
            .args(["set-volume", "@DEFAULT_AUDIO_SINK@", &arg])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
}

/// Toggle mute on the default sink.
pub fn toggle_mute() {
    hytte_reactive::runtime::handle().spawn_blocking(|| {
        let _ = Command::new("wpctl")
            .args(["set-mute", "@DEFAULT_AUDIO_SINK@", "toggle"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn parse_pactl_info_field() {
        let info = "Server String: /run/user/1000/pulse/native\nDefault Sink: alsa_output.pci\nDefault Source: alsa_input.pci\n";
        assert_eq!(
            super::parse_pactl_info_field(info, "Default Sink"),
            Some("alsa_output.pci".to_string())
        );
        assert_eq!(
            super::parse_pactl_info_field(info, "Default Source"),
            Some("alsa_input.pci".to_string())
        );
    }

    #[test]
    fn parse_sink_input_blocks() {
        // Block 0: classic case with application.name set.
        // Block 1: Spotify-shaped — no application.name, only node.name + media.name.
        // Block 2: nothing useful at all → fall back to "Stream {id}".
        let input = "Sink Input #51\n\
\tDriver: PipeWire\n\
\tClient: 194\n\
\tSink: 0\n\
\tVolume: front-left: 32768 / 50% / -6.02 dB,   front-right: 32768 / 50% / -6.02 dB\n\
\tMute: no\n\
\tProperties:\n\
\t\tapplication.name = \"Firefox\"\n\
\t\tapplication.process.binary = \"firefox\"\n\
\t\tmedia.name = \"Playback\"\n\
\n\
Sink Input #9220\n\
\tDriver: PipeWire\n\
\tClient: 200\n\
\tSink: 1\n\
\tVolume: front-left: 65536 / 100% / 0.00 dB,   front-right: 65536 / 100% / 0.00 dB\n\
\tMute: yes\n\
\tProperties:\n\
\t\tnode.name = \"audio-src\"\n\
\t\tmedia.name = \"Sweet Caroline — Neil Diamond\"\n\
\n\
Sink Input #77\n\
\tDriver: PipeWire\n\
\tClient: 300\n\
\tSink: 0\n\
\tProperties:\n\
\t\tobject.serial = \"99\"\n";
        let streams = super::parse_sink_input_blocks_with_ids(input);
        assert_eq!(streams.len(), 3);

        assert_eq!(streams[0].id, 51);
        assert_eq!(streams[0].app_name, "Firefox");
        assert_eq!(streams[0].sink_id, 0);
        assert!((streams[0].volume - 0.5).abs() < 1e-9);
        assert!(!streams[0].muted);

        assert_eq!(streams[1].id, 9220);
        // node.name="audio-src" is generic and skipped; media.name wins.
        assert_eq!(streams[1].app_name, "Sweet Caroline — Neil Diamond");
        assert_eq!(streams[1].sink_id, 1);
        assert!((streams[1].volume - 1.0).abs() < 1e-9);
        assert!(streams[1].muted);

        assert_eq!(streams[2].app_name, "Stream 77");
    }

    #[test]
    fn parse_pactl_volume_stereo() {
        let line = "front-left: 32768 /  50% / -6.02 dB,   front-right: 32768 /  50% / -6.02 dB";
        let v = super::parse_pactl_volume(line).unwrap();
        assert!((v - 0.5).abs() < 1e-9);
    }

    #[test]
    fn parse_pactl_volume_mono_full() {
        let line = "mono: 65536 / 100% / 0.00 dB";
        let v = super::parse_pactl_volume(line).unwrap();
        assert!((v - 1.0).abs() < 1e-9);
    }

    #[test]
    fn parse_pactl_volume_garbage() {
        assert!(super::parse_pactl_volume("not a volume line").is_none());
    }

    #[test]
    fn parse_pactl_mute_yes_no() {
        assert!(super::parse_pactl_mute("yes"));
        assert!(super::parse_pactl_mute("  yes  "));
        assert!(!super::parse_pactl_mute("no"));
    }

    #[test]
    fn is_relevant_event_filters_categories() {
        assert!(super::is_relevant_event("Event 'change' on sink #0"));
        assert!(super::is_relevant_event("Event 'new' on sink-input #9220"));
        assert!(super::is_relevant_event("Event 'remove' on source-output #5"));
        assert!(super::is_relevant_event("Event 'change' on source #1"));
        assert!(super::is_relevant_event("Event 'change' on server"));
        assert!(!super::is_relevant_event("Event 'change' on client #200"));
        assert!(!super::is_relevant_event("Event 'change' on card #42"));
        assert!(!super::is_relevant_event("Event 'change' on module #50"));
        assert!(!super::is_relevant_event("garbage line"));
    }

    #[test]
    fn monitor_sources_filtered() {
        let short = "0\talsa_input.pci-good\tPipeWire\ts32le 2ch 48000Hz\tSUSPENDED\n2\talsa_output.pci.monitor\tPipeWire\ts32le 2ch 48000Hz\tSUSPENDED\n";
        let non_monitor: Vec<&str> = short
            .lines()
            .filter(|l| {
                let parts: Vec<&str> = l.splitn(3, '\t').collect();
                parts.len() >= 2 && !parts[1].ends_with(".monitor")
            })
            .collect();
        assert_eq!(non_monitor.len(), 1);
        assert!(non_monitor[0].contains("alsa_input.pci-good"));
    }
}
