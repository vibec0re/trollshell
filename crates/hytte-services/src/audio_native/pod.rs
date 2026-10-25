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

/// The gain a volume step raises a channel to at most — unity, 100 % — unless
/// that channel already sits higher or the caller asked for more.
///
/// There is no per-channel bound anywhere else in this module to copy:
/// `cast::f64_to_f32_gain` is a plain narrowing, and the *level* a caller
/// requests is bounded by the caller (the drawer's slider tops out at `1.0`,
/// `plugins::effects::clamp_volume` clamps a plugin's request to `1.0`, and
/// the bar chip's scroll stops at that same `MAX_VOLUME`). What needs a bound
/// here is new with
/// [`scaled_channel_volumes`]: keeping the balance means a request for 100 %
/// on an unbalanced sink would otherwise take its loud channel *past* 100 % —
/// a software boost no surface asked for. The two exceptions keep a step a
/// step: a channel already above unity (the shipped keybinds boost with
/// `wpctl set-volume … -l 1.5`) is not snapped down to it, and an explicit
/// request above unity (this crate's setters take a boost) still gets one.
const UNITY_GAIN: f64 = 1.0;

/// Per-channel gains for a requested overall level that **keep the balance**.
///
/// The level every surface shows is the channel average ([`avg_volume`]), so a
/// request for `target` is a request for that average. Until this existed the
/// backend wrote `target` to every channel, which flattened a balanced-off
/// sink on the first touch: L = 1.0, R = 0.5 shows 75 %, and one scroll notch
/// set both to 0.80. Now each channel is scaled by `target / avg`, so the
/// ratios survive — that sink asked for 60 % becomes L = 0.8, R = 0.4.
///
/// Scaling up can push a channel into its cap ([`UNITY_GAIN`], or higher where
/// the channel or the request already is). A channel that would cross it is
/// **pinned** at it and the rest of the requested total is shared out over the
/// unpinned channels in proportion to their gains, repeated until nothing new
/// pins — so the average still lands on `target` exactly (that sink asked for
/// 80 % becomes L = 1.0, R = 0.6), and the balance only gives way where the
/// ceiling forces it. Simply clamping each channel instead would leave the
/// average short of every request near the top, so the shown level would
/// approach 100 % by halving the gap on each notch and never quite arrive. A
/// channel at zero cannot be raised by scaling, so while every unpinned channel
/// is silent what is left is split evenly between them (a sink panned hard
/// left, L = 1.0, R = 0.0, asked for 60 % gets R = 0.2): the alternative is a
/// control that does nothing.
///
/// All-silent channels (or a zero average) carry no balance to keep, and get
/// `target` each, as the old code did. `None` for an empty slice (the node's
/// layout is not known yet, and a guessed mono array would clobber it) and for
/// a non-finite `target`, which has no defensible gain. Total by construction —
/// nothing here indexes, unwraps or calls `f64::clamp` (which panics on a NaN
/// bound) — because it runs inside a `PipeWire` callback, where a panic aborts
/// the process.
pub(super) fn scaled_channel_volumes(current: &[f32], target: f64) -> Option<Vec<f32>> {
    /// One channel's state through the share-out.
    struct Channel {
        gain: f64,
        cap: f64,
        pinned: bool,
    }

    if current.is_empty() || !target.is_finite() {
        return None;
    }
    let target = target.max(0.0);
    let count = crate::cast::usize_to_f64(current.len());
    // `max(0.0)` also turns a NaN off the wire into silence.
    let mut channels: Vec<Channel> = current
        .iter()
        .map(|g| {
            let gain = f64::from(*g).max(0.0);
            Channel {
                gain,
                cap: gain.max(UNITY_GAIN).max(target),
                pinned: false,
            }
        })
        .collect();
    if channels.iter().map(|c| c.gain).sum::<f64>() <= 0.0 {
        return Some(vec![crate::cast::f64_to_f32_gain(target); current.len()]);
    }

    let wanted = target * count;
    loop {
        let (pinned_total, free_gain, free_count) =
            channels
                .iter()
                .fold((0.0_f64, 0.0_f64, 0_usize), |(p, g, n), c| {
                    if c.pinned {
                        (p + c.cap, g, n)
                    } else {
                        (p, g + c.gain, n + 1)
                    }
                });
        let left = (wanted - pinned_total).max(0.0);
        let level = |c: &Channel| {
            if free_gain > 0.0 {
                c.gain * left / free_gain
            } else {
                left / crate::cast::usize_to_f64(free_count.max(1))
            }
        };
        let mut pinned_more = false;
        for c in &mut channels {
            if !c.pinned && level(c) > c.cap {
                c.pinned = true;
                pinned_more = true;
            }
        }
        // Each pass either pins at least one more channel or returns, so this
        // ends within `current.len() + 1` passes.
        if !pinned_more {
            return Some(
                channels
                    .iter()
                    .map(|c| crate::cast::f64_to_f32_gain(if c.pinned { c.cap } else { level(c) }))
                    .collect(),
            );
        }
    }
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

    /// `scaled_channel_volumes`, asserted channel by channel to f32 precision.
    fn assert_gains(current: &[f32], target: f64, expected: &[f32]) {
        let got =
            scaled_channel_volumes(current, target).expect("a finite target on a known layout");
        assert_eq!(got.len(), expected.len(), "{current:?} → {target}: {got:?}");
        for (g, e) in got.iter().zip(expected) {
            assert!(
                (g - e).abs() < 1e-6,
                "{current:?} → {target}: got {got:?}, want {expected:?}",
            );
        }
    }

    /// The bug: setting the level wrote it to every channel, so the first touch
    /// on a balanced-off sink flattened it — L = 1.0, R = 0.5 shows 75 %, and
    /// one notch set both to 0.80. Lowering it now keeps the 2:1 ratio.
    ///
    /// Falsify by giving `scaled_channel_volumes` the old `send_volume` body,
    /// `vec![target; current.len()]`: the first two rows come back flat
    /// (`[0.6, 0.6]`, `[0.75, 0.75]`).
    #[test]
    fn a_volume_step_keeps_the_balance() {
        assert_gains(&[1.0, 0.5], 0.6, &[0.8, 0.4]);
        assert_gains(&[0.8, 0.4], 0.75, &[1.0, 0.5]);
        // A balanced sink stays balanced, and mono is just the level.
        assert_gains(&[0.5, 0.5], 0.8, &[0.8, 0.8]);
        assert_gains(&[0.3], 0.7, &[0.7]);
    }

    /// The reported level is the average, so the average must land on the
    /// request — for every layout, including one where a channel pins at the
    /// ceiling. Simply clamping each channel instead leaves it short near the
    /// top, and the shown level then creeps toward 100 % by halving the gap.
    #[test]
    fn the_average_lands_on_the_requested_level() {
        for (current, target) in [
            (&[1.0_f32, 0.5][..], 0.8),
            (&[1.0, 0.5][..], 1.0),
            (&[0.9, 0.1, 0.5, 0.5, 0.2, 0.7][..], 0.95),
            (&[0.25, 0.75][..], 0.1),
            (&[1.0, 0.0][..], 0.6),
        ] {
            let got = scaled_channel_volumes(current, target).expect("scaled");
            let avg = avg_volume(&got);
            assert!(
                (avg - target).abs() < 1e-6,
                "{current:?} → {target}: {got:?} averages {avg}",
            );
        }
    }

    /// Raising an unbalanced sink to the top must not take its loud channel past
    /// 100 % — a boost nobody asked for. It pins at unity and the quiet channel
    /// takes up the rest, so 80 % is L = 1.0, R = 0.6, and 100 % is both at 1.0.
    #[test]
    fn a_channel_pins_at_unity_rather_than_being_boosted() {
        assert_gains(&[1.0, 0.5], 0.8, &[1.0, 0.6]);
        assert_gains(&[1.0, 0.5], 1.0, &[1.0, 1.0]);
        let got = scaled_channel_volumes(&[0.9, 0.1, 0.5, 0.5, 0.2, 0.7], 0.95).expect("scaled");
        assert!(
            got.iter().all(|g| *g <= 1.0 + 1e-6),
            "a 5.1 sink raised to 95 % boosts a channel: {got:?}",
        );
    }

    /// A boost made elsewhere (the shipped keybinds' `wpctl … -l 1.5`) is not
    /// snapped down to unity by a step: lowering scales it like anything else.
    /// And an explicit request above unity — the setters take a boost — still
    /// gets one.
    #[test]
    fn a_boost_is_lowered_by_a_step_and_an_explicit_boost_is_kept() {
        assert_gains(&[1.5, 1.0], 1.2, &[1.44, 0.96]);
        assert_gains(&[1.0, 1.0], 1.2, &[1.2, 1.2]);
    }

    /// A silent channel cannot be raised by scaling, so a sink panned hard
    /// left still answers a raise — the leftover goes to the silent side
    /// rather than the control doing nothing — while a lowering keeps the pan.
    #[test]
    fn a_hard_pan_still_answers_a_raise() {
        assert_gains(&[1.0, 0.0], 0.6, &[1.0, 0.2]);
        assert_gains(&[1.0, 0.0], 0.4, &[0.8, 0.0]);
        // All silent: no balance to keep, every channel takes the level.
        assert_gains(&[0.0, 0.0], 0.3, &[0.3, 0.3]);
    }

    /// Nothing to send without a layout (a guessed mono array would clobber a
    /// stereo sink's) or for a level with no defensible gain; a negative
    /// request is silence; a NaN gain off the wire reads as silence too.
    #[test]
    fn scaling_refuses_what_it_cannot_honour() {
        assert!(scaled_channel_volumes(&[], 0.5).is_none());
        assert!(scaled_channel_volumes(&[0.5, 0.5], f64::NAN).is_none());
        assert!(scaled_channel_volumes(&[0.5, 0.5], f64::INFINITY).is_none());
        assert_gains(&[0.5, 0.25], -1.0, &[0.0, 0.0]);
        assert_gains(&[f32::NAN, 0.5], 0.5, &[0.0, 1.0]);
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
