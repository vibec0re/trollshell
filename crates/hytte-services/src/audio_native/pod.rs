//! Pure `SPA` pod serialization and parsing helpers.
//!
//! All functions in this module are stateless and depend only on the
//! `pipewire` / `libspa` crates. They are `pub(super)` so the mainloop
//! can call them, and independently unit-testable with `cargo test`.

use pipewire as pw;

/// Serialize a `SPA_TYPE_OBJECT_Props` pod with whichever of channelVolumes
/// and mute are supplied. Returns the raw byte buffer, which the caller
/// wraps with `Pod::from_bytes` before passing to `set_param`. Both fields
/// optional so we can issue volume-only and mute-only updates without
/// clobbering the other.
pub(super) fn build_props_pod(
    channel_volumes: Option<Vec<f32>>,
    mute: Option<bool>,
) -> Option<Vec<u8>> {
    use pw::spa::pod::{Object, Property, Value, ValueArray, serialize::PodSerializer};
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
pub(super) fn decode_props(bytes: &[u8]) -> Option<(Option<Vec<f32>>, Option<bool>)> {
    use pw::spa::pod::{Value, ValueArray, deserialize::PodDeserializer};
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

/// Average of per-channel linear gains. `PipeWire` spec doesn't require all
/// channels to agree (you can pan via uneven channelVolumes), but pactl's
/// historical convention — which the UI is calibrated against — reports
/// the first channel's value. Averaging is friendlier when the UI shows
/// a single slider for a stereo sink: a 100%/0% pair reads 50% instead of
/// 100%, matching what the user perceives. Empty array → 0.0.
pub(super) fn avg_volume(channels: &[f32]) -> f64 {
    if channels.is_empty() {
        return 0.0;
    }
    let sum: f64 = channels.iter().map(|v| f64::from(*v)).sum();
    sum / crate::cast::usize_to_f64(channels.len())
}

/// Extract `node.name` from a `default.audio.{sink,source}` metadata
/// payload. `PipeWire` encodes these as JSON: `{"name":"<node.name>"}`.
/// Returns `None` if the JSON is malformed or missing the `name` field —
/// safer than guessing, the previous default just stays in place.
pub(super) fn parse_default_name(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    v.get("name")?.as_str().map(str::to_owned)
}

/// Choose the user-facing app name for a stream's props dict. Mirrors the
/// fallback chain `super::pipewire::pick_app_name` uses on `pactl list
/// sink-inputs` output — Spotify in particular publishes only `node.name`
/// over the pipewire-pulse compat layer, and several generic placeholders
/// must be filtered so a useful `media.name` further down the list wins.
pub(super) fn pick_app_name(props: &pw::spa::utils::dict::DictRef) -> String {
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

    #[test]
    fn avg_volume_empty_is_zero() {
        // A node we haven't received Props for yet has an empty
        // channelVolumes Vec. The UI reads `volume` and divides by
        // implicit ranges; producing 0.0 keeps it safely off the rails.
        // The empty-input path returns the literal constant 0.0 — use an
        // epsilon comparison to stay consistent with float-comparison style.
        assert!(avg_volume(&[]).abs() < f64::EPSILON);
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
}
