//! Default audio sink volume + mute state, polled via `wpctl`.
//!
//! v0.2.0 uses a 250 ms shell-out poll for simplicity. v0.3+ should
//! switch to a proper `pipewire-rs` registry subscription so updates
//! arrive event-driven.
//!
//! v0.7.1 extends this to full sink/source/sink-input tracking via
//! pactl, polled at 1 Hz.

use futures_signals::signal::{Mutable, Signal};
use hytte_reactive::{registry, Service};
use std::collections::HashMap;
use std::process::Command;
use std::time::Duration;

pub struct PipewireService;

#[derive(Clone, Copy, Debug, Default)]
pub struct Volume {
    /// Linear volume, `0.0..=1.0` (may exceed 1.0 if user boosts above
    /// 100%). Untouched on parse failure.
    pub linear: f64,
    pub muted: bool,
}

#[derive(Clone, Debug)]
pub struct Sink {
    pub id: u32,
    pub name: String,
    pub description: String,
    pub volume: f64,
    pub muted: bool,
    pub is_default: bool,
}

#[derive(Clone, Debug)]
pub struct Source {
    pub id: u32,
    pub name: String,
    pub description: String,
    pub volume: f64,
    pub muted: bool,
    pub is_default: bool,
}

#[derive(Clone, Debug)]
pub struct PlaybackStream {
    pub id: u32,
    pub app_name: String,
    pub sink_id: u32,
    pub volume: f64,
    pub muted: bool,
}

#[doc(hidden)]
pub struct PipewireHandles {
    pub(crate) sink: Mutable<Volume>,
    pub(crate) sinks: Mutable<Vec<Sink>>,
    pub(crate) sources: Mutable<Vec<Source>>,
    pub(crate) streams: Mutable<Vec<PlaybackStream>>,
}

impl Default for PipewireHandles {
    fn default() -> Self {
        Self {
            sink: Mutable::new(Volume::default()),
            sinks: Mutable::new(Vec::new()),
            sources: Mutable::new(Vec::new()),
            streams: Mutable::new(Vec::new()),
        }
    }
}

impl Service for PipewireService {
    type Handles = PipewireHandles;

    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles {
        let handles = PipewireHandles::default();

        // 250 ms default-sink poll (bar widget depends on this).
        let writer = handles.sink.clone();
        rt.spawn(async move {
            let mut last = Volume::default();
            loop {
                if let Some(v) = poll() {
                    #[allow(clippy::float_cmp)]
                    if v.linear != last.linear || v.muted != last.muted {
                        writer.set(v);
                        last = v;
                    }
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        });

        // 1 Hz full state poll for sinks/sources/streams.
        let sinks_writer = handles.sinks.clone();
        let sources_writer = handles.sources.clone();
        let streams_writer = handles.streams.clone();
        rt.spawn(async move {
            loop {
                if let Some(state) = read_full_state() {
                    sinks_writer.set(state.sinks);
                    sources_writer.set(state.sources);
                    streams_writer.set(state.streams);
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });

        handles
    }
}

// ── pactl parsing ─────────────────────────────────────────────────────────────

struct FullState {
    sinks: Vec<Sink>,
    sources: Vec<Source>,
    streams: Vec<PlaybackStream>,
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
    let sink_descriptions = parse_descriptions_from_long(&sinks_long_out, "Sink #");

    let mut sinks = Vec::new();
    for line in sinks_short.lines() {
        let parts: Vec<&str> = line.splitn(5, '\t').collect();
        if parts.len() < 2 {
            continue;
        }
        let id: u32 = parts[0].trim().parse().ok()?;
        let name = parts[1].trim().to_string();
        let description = sink_descriptions
            .get(&name)
            .cloned()
            .unwrap_or_else(|| name.clone());
        let is_default = default_sink_name.as_deref() == Some(name.as_str());
        let (volume, muted) = get_volume_mute(id);
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
    let source_descriptions = parse_descriptions_from_long(&sources_long_out, "Source #");

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
        let description = source_descriptions
            .get(&name)
            .cloned()
            .unwrap_or_else(|| name.clone());
        let is_default = default_source_name.as_deref() == Some(name.as_str());
        let (volume, muted) = get_volume_mute(id);
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

    Some(FullState {
        sinks,
        sources,
        streams,
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

/// Build a map of `name → description` from long-form pactl sink/source output.
fn parse_descriptions_from_long(output: &str, block_prefix: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for block in parse_pactl_blocks(output, block_prefix) {
        if let (Some(name), Some(desc)) = (block.get("Name"), block.get("Description")) {
            map.insert(name.clone(), desc.clone());
        }
    }
    map
}

fn parse_playback_streams() -> Vec<PlaybackStream> {
    let Some(long_out) = run_cmd(&["pactl", "list", "sink-inputs"]) else {
        return Vec::new();
    };
    parse_sink_input_blocks_with_ids(&long_out)
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
    let app_name = map
        .get("application.name")
        .cloned()
        .or_else(|| map.get("application.process.binary").cloned())
        .unwrap_or_else(|| format!("Stream {id}"));
    let (volume, muted) = get_volume_mute(id);
    Some(PlaybackStream {
        id,
        app_name,
        sink_id,
        volume,
        muted,
    })
}

fn get_volume_mute(id: u32) -> (f64, bool) {
    let id_str = id.to_string();
    let out = Command::new("wpctl")
        .args(["get-volume", &id_str])
        .output()
        .ok();
    match out {
        Some(o) if o.status.success() => {
            let s = std::str::from_utf8(&o.stdout).unwrap_or("");
            parse(s).map_or((0.0, false), |v| (v.linear, v.muted))
        }
        _ => (0.0, false),
    }
}

fn poll() -> Option<Volume> {
    let out = Command::new("wpctl")
        .args(["get-volume", "@DEFAULT_AUDIO_SINK@"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = std::str::from_utf8(&out.stdout).ok()?;
    parse(s)
}

fn parse(s: &str) -> Option<Volume> {
    // Expected: "Volume: 0.65 [MUTED]\n" or "Volume: 0.65\n"
    let trimmed = s.trim();
    let rest = trimmed.strip_prefix("Volume:")?.trim();
    let mut parts = rest.split_whitespace();
    let linear: f64 = parts.next()?.parse().ok()?;
    let muted = rest.contains("[MUTED]");
    Some(Volume { linear, muted })
}

// ── Commands (fire-and-forget) ─────────────────────────────────────────────────

fn spawn_cmd(mut cmd: Command) {
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

pub fn set_sink_volume(id: u32, linear: f64) {
    let v = format!("{linear:.4}");
    let id_str = id.to_string();
    spawn_cmd({
        let mut c = Command::new("wpctl");
        c.args(["set-volume", &id_str, &v]);
        c
    });
}

pub fn set_source_volume(id: u32, linear: f64) {
    let v = format!("{linear:.4}");
    let id_str = id.to_string();
    spawn_cmd({
        let mut c = Command::new("wpctl");
        c.args(["set-volume", &id_str, &v]);
        c
    });
}

pub fn set_stream_volume(id: u32, linear: f64) {
    let v = format!("{linear:.4}");
    let id_str = id.to_string();
    spawn_cmd({
        let mut c = Command::new("wpctl");
        c.args(["set-volume", &id_str, &v]);
        c
    });
}

pub fn set_sink_mute(id: u32, mute: bool) {
    let m = if mute { "1" } else { "0" };
    let id_str = id.to_string();
    spawn_cmd({
        let mut c = Command::new("wpctl");
        c.args(["set-mute", &id_str, m]);
        c
    });
}

pub fn set_source_mute(id: u32, mute: bool) {
    let m = if mute { "1" } else { "0" };
    let id_str = id.to_string();
    spawn_cmd({
        let mut c = Command::new("wpctl");
        c.args(["set-mute", &id_str, m]);
        c
    });
}

pub fn set_stream_mute(id: u32, mute: bool) {
    let m = if mute { "1" } else { "0" };
    let id_str = id.to_string();
    spawn_cmd({
        let mut c = Command::new("wpctl");
        c.args(["set-mute", &id_str, m]);
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

/// Set the default sink's linear volume (0.0..=1.0+). Fire-and-forget;
/// runs `wpctl set-volume` on the tokio runtime so the GTK main thread
/// stays responsive during continuous slider drags.
pub fn set_volume(linear: f64) {
    hytte_reactive::runtime::handle().spawn_blocking(move || {
        let arg = format!("{linear:.4}");
        let _ = Command::new("wpctl")
            .args(["set-volume", "@DEFAULT_AUDIO_SINK@", &arg])
            .status();
    });
}

/// Toggle mute on the default sink.
pub fn toggle_mute() {
    hytte_reactive::runtime::handle().spawn_blocking(|| {
        let _ = Command::new("wpctl")
            .args(["set-mute", "@DEFAULT_AUDIO_SINK@", "toggle"])
            .status();
    });
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn parse_unmuted() {
        let v = parse("Volume: 0.65\n").unwrap();
        assert!((v.linear - 0.65).abs() < 1e-9);
        assert!(!v.muted);
    }

    #[test]
    fn parse_muted() {
        let v = parse("Volume: 0.20 [MUTED]\n").unwrap();
        assert!((v.linear - 0.20).abs() < 1e-9);
        assert!(v.muted);
    }

    #[test]
    fn parse_garbage_returns_none() {
        assert!(parse("not wpctl output").is_none());
        assert!(parse("Volume: foo").is_none());
    }

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
        let input = r#"Sink Input #51
	Driver: PipeWire
	Client: 194
	Sink: 0
	Properties:
		application.name = "Firefox"
		application.process.binary = "firefox"
		media.name = "Playback"

Sink Input #52
	Driver: PipeWire
	Client: 200
	Sink: 1
	Properties:
		application.process.binary = "spotify"
		media.name = "Music"
"#;
        let streams = super::parse_sink_input_blocks_with_ids(input);
        // No wpctl available in test, so volume/muted will be defaults.
        assert_eq!(streams.len(), 2);
        assert_eq!(streams[0].id, 51);
        assert_eq!(streams[0].app_name, "Firefox");
        assert_eq!(streams[0].sink_id, 0);
        assert_eq!(streams[1].id, 52);
        assert_eq!(streams[1].app_name, "spotify");
        assert_eq!(streams[1].sink_id, 1);
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
