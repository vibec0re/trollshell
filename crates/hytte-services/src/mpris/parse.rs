//! Pure parsers for untrusted MPRIS `Metadata` (`a{sv}`) payloads.
//!
//! Everything here is free of I/O: each function takes an in-memory
//! `zvariant` value fetched elsewhere (the `org.mpris.MediaPlayer2.Player`
//! `Metadata` map or one of its entries) and returns typed Rust data,
//! defaulting rather than panicking on the malformed input arbitrary media
//! players emit. That makes the whole module independently unit-testable in
//! the hermetic (`cargo test`) bucket — see the `tests` module below.
//!
//! The bus call that fetches the raw `Metadata` value lives in [`super`]; only
//! its pure map-extraction half ([`parse_metadata`]) and the three per-field
//! parsers it delegates to are here.

use std::collections::HashMap;
use zbus::zvariant::OwnedValue;

/// Extract track metadata from a raw `Metadata` property value.
///
/// The value should be an `a{sv}` map; anything else — a non-map value, or
/// missing keys / wrong-typed values inside it — yields all-default fields
/// rather than an error or a panic. Returns
/// `(title, artists, album, art_url, length_us, track_id)`.
///
/// This is the pure map-extraction half of the service's `read_metadata`; the
/// bus call that fetches `raw` is the (I/O-touching) orchestrator in [`super`].
pub(super) fn parse_metadata(
    raw: OwnedValue,
) -> (String, String, String, String, u64, Option<String>) {
    let Ok(map) = HashMap::<String, OwnedValue>::try_from(raw) else {
        return (
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            0,
            None,
        );
    };

    let title = map
        .get("xesam:title")
        .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
        .unwrap_or_default();

    let album = map
        .get("xesam:album")
        .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
        .unwrap_or_default();

    // xesam:artist — a string array (as), or a bare string from players that
    // ignore the spec; handle gracefully if absent or malformed.
    let artists = parse_artist_array(map.get("xesam:artist"));

    // xesam:artUrl — a plain string in most players.
    let art_url = map
        .get("xesam:artUrl")
        .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
        .unwrap_or_default();

    // mpris:length — u64 or i64 microseconds.
    let length_us = parse_length(map.get("mpris:length"));

    // mpris:trackid — ObjectPath or String.
    let track_id = parse_track_id(map.get("mpris:trackid"));

    (title, artists, album, art_url, length_us, track_id)
}

/// Parse `xesam:artist` from an `OwnedValue`. The spec says `as` (array of
/// strings), but some players send a bare `s` instead, so fall back to reading
/// the value as a plain String — the same widening the sibling parsers already
/// do ([`parse_track_id`] accepts `ObjectPath` or `String`, [`parse_length`]
/// `u64` or `i64`). Anything else yields the empty string.
fn parse_artist_array(val: Option<&OwnedValue>) -> String {
    let Some(v) = val else { return String::new() };
    let Ok(owned) = v.try_clone() else {
        return String::new();
    };

    // Spec-compliant `as` first.
    if let Ok(arr) = zbus::zvariant::Array::try_from(owned.clone()) {
        let parts: Vec<String> = arr
            .iter()
            .filter_map(|item| {
                let cloned = item.try_clone().ok()?;
                String::try_from(OwnedValue::try_from(cloned).ok()?).ok()
            })
            .collect();
        return parts.join(", ");
    }

    // Bare `s` — surface it verbatim rather than dropping the artist.
    String::try_from(owned).unwrap_or_default()
}

/// Parse `mpris:length` from an `OwnedValue`. The spec says u64 but some
/// players send i64. Saturate negatives to 0.
fn parse_length(val: Option<&OwnedValue>) -> u64 {
    let Some(v) = val else { return 0 };
    let Ok(owned) = v.try_clone() else { return 0 };

    // Try u64 first (spec-compliant).
    if let Ok(n) = u64::try_from(owned.clone()) {
        return n;
    }
    // Fall back to i64, saturate negatives.
    if let Ok(n) = i64::try_from(owned) {
        return u64::try_from(n).unwrap_or(0);
    }
    0
}

/// Parse `mpris:trackid` from an `OwnedValue`. May be an `ObjectPath`, a plain
/// String, or a Variant wrapping one of those (some players double-wrap the
/// entry in a nested `Value::Value` rather than sending a bare
/// `ObjectPath`/`String`). Returns the underlying path/string as a `String`,
/// or `None` if absent or unparseable.
fn parse_track_id(val: Option<&OwnedValue>) -> Option<String> {
    let v = val?;
    let Ok(owned) = v.try_clone() else {
        return None;
    };
    let owned = unwrap_variant(owned)?;

    // Try ObjectPath first (most common).
    if let Ok(path) = zbus::zvariant::OwnedObjectPath::try_from(owned.clone()) {
        return Some(path.as_str().to_string());
    }
    // Try plain String.
    if let Ok(s) = String::try_from(owned) {
        return Some(s);
    }
    None
}

/// Peel any `Value::Value` variant wrapping around `value`. Some MPRIS
/// players deliver `mpris:trackid` doubly-wrapped in a Variant rather than as
/// a bare `ObjectPath`/`String`; without peeling, `OwnedObjectPath::try_from`
/// / `String::try_from` see the wrapper type and fail. Mirrors tray parse's
/// `unwrap_variant` (#259 / `crates/hytte-services/src/tray/parse.rs`), and
/// loops to handle repeated wrapping. Returns `None` (rather than panicking)
/// if re-owning the peeled value fails.
fn unwrap_variant(value: OwnedValue) -> Option<OwnedValue> {
    let mut inner: zbus::zvariant::Value<'static> = value.into();
    while let zbus::zvariant::Value::Value(boxed) = inner {
        inner = *boxed;
    }
    OwnedValue::try_from(inner).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbus::zvariant::{ObjectPath, Value};

    // ── Fixture builders ──────────────────────────────────────────────────────

    /// Serialise any `Into<Value>` into an `OwnedValue`, mirroring what zbus
    /// hands back from a property read.
    fn owned<T: Into<Value<'static>>>(v: T) -> OwnedValue {
        v.into().try_to_owned().expect("fixture must serialise")
    }

    /// Build an `a{sv}` `Metadata` map from `(key, value)` entries — the exact
    /// shape zbus deserialises the `Metadata` property into.
    fn metadata(entries: Vec<(&str, Value<'static>)>) -> OwnedValue {
        let map: HashMap<String, Value<'static>> = entries
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        Value::from(map)
            .try_to_owned()
            .expect("metadata dict must serialise")
    }

    // ── parse_length: i64 / negative / string / missing ───────────────────────

    #[test]
    fn length_u64_is_returned() {
        assert_eq!(parse_length(Some(&owned(1_234_567_u64))), 1_234_567);
    }

    #[test]
    fn length_positive_i64_is_returned() {
        // Spec says u64, but many players emit i64; a positive value survives.
        assert_eq!(parse_length(Some(&owned(9_000_i64))), 9_000);
    }

    #[test]
    fn length_negative_i64_saturates_to_zero() {
        assert_eq!(parse_length(Some(&owned(-42_i64))), 0);
    }

    #[test]
    fn length_as_string_defaults_to_zero() {
        // A player that (wrongly) sends the length as a string must not panic.
        assert_eq!(parse_length(Some(&owned("123456"))), 0);
    }

    #[test]
    fn length_missing_is_zero() {
        assert_eq!(parse_length(None), 0);
    }

    // ── parse_artist_array: array vs bare string vs missing ───────────────────

    #[test]
    fn artist_array_joins_with_comma() {
        let arr = owned(vec!["Alice", "Bob", "Carol"]);
        assert_eq!(parse_artist_array(Some(&arr)), "Alice, Bob, Carol");
    }

    #[test]
    fn artist_single_element_array() {
        let arr = owned(vec!["Solo"]);
        assert_eq!(parse_artist_array(Some(&arr)), "Solo");
    }

    #[test]
    fn artist_empty_array_is_empty() {
        let arr = owned(Vec::<&str>::new());
        assert_eq!(parse_artist_array(Some(&arr)), "");
    }

    /// Some players send `xesam:artist` as a bare string rather than the spec's
    /// `as` array. `Array::try_from` fails on it, so we fall back to
    /// `String::try_from` and surface it verbatim — the same widening
    /// `parse_track_id` (`ObjectPath` | `String`) and `parse_length`
    /// (`u64` | `i64`) already do. Before #651 this dropped the artist
    /// everywhere: bar chip, Media panel, and the `NowPlaying` push to plugins.
    #[test]
    fn artist_bare_string_is_surfaced() {
        assert_eq!(
            parse_artist_array(Some(&owned("Bare Artist"))),
            "Bare Artist"
        );
    }

    #[test]
    fn artist_bare_empty_string_is_empty() {
        assert_eq!(parse_artist_array(Some(&owned(""))), "");
    }

    #[test]
    fn artist_wrong_scalar_type_is_empty() {
        // Neither an `as` nor an `s` — default rather than panic.
        assert_eq!(parse_artist_array(Some(&owned(42_u64))), "");
    }

    #[test]
    fn artist_missing_is_empty() {
        assert_eq!(parse_artist_array(None), "");
    }

    // ── parse_track_id: ObjectPath / String / Variant-wrapped / garbage ───────

    #[test]
    fn track_id_object_path() {
        let op = ObjectPath::try_from("/com/spotify/track/abc").expect("valid path");
        let id = parse_track_id(Some(&owned(op)));
        assert_eq!(id.as_deref(), Some("/com/spotify/track/abc"));
    }

    #[test]
    fn track_id_plain_string() {
        // Not necessarily a valid object path — arrives as a Str; the String
        // branch still surfaces it verbatim.
        let id = parse_track_id(Some(&owned("/players/track/7")));
        assert_eq!(id.as_deref(), Some("/players/track/7"));
    }

    /// The doc comment promises a "Variant wrapping one of those" is handled;
    /// `parse_track_id` now peels a nested `Value::Value` via `unwrap_variant`
    /// (mirroring tray parse's #259 fix) before trying `OwnedObjectPath`/
    /// `String`, so a variant-wrapped trackid resolves to the wrapped path —
    /// closing the doc-vs-code gap the #233 triage flagged.
    #[test]
    fn track_id_variant_wrapped_object_path_peeled() {
        let op = ObjectPath::try_from("/nested/track/1").expect("valid path");
        let wrapped = Value::Value(Box::new(Value::from(op)));
        let owned_wrapped = wrapped.try_to_owned().expect("variant serialises");
        assert_eq!(
            parse_track_id(Some(&owned_wrapped)).as_deref(),
            Some("/nested/track/1")
        );
    }

    /// `unwrap_variant` loops rather than peeling a single layer, so a
    /// doubly-wrapped trackid (`Value::Value(Value::Value(ObjectPath))`) also
    /// resolves — some players/proxies nest the wrapper more than once.
    #[test]
    fn track_id_double_variant_wrapped_object_path_peeled() {
        let op = ObjectPath::try_from("/nested/track/2").expect("valid path");
        let wrapped = Value::Value(Box::new(Value::Value(Box::new(Value::from(op)))));
        let owned_wrapped = wrapped.try_to_owned().expect("variant serialises");
        assert_eq!(
            parse_track_id(Some(&owned_wrapped)).as_deref(),
            Some("/nested/track/2")
        );
    }

    #[test]
    fn track_id_garbage_type_is_none() {
        // A bool where a path/string is expected → None, no panic.
        assert_eq!(parse_track_id(Some(&owned(true))), None);
    }

    #[test]
    fn track_id_missing_is_none() {
        assert_eq!(parse_track_id(None), None);
    }

    // ── parse_metadata: end-to-end over the a{sv} map ─────────────────────────

    #[test]
    fn metadata_full_dict_extracts_all_fields() {
        let op = ObjectPath::try_from("/track/42").expect("valid path");
        let raw = metadata(vec![
            ("xesam:title", Value::from("Song")),
            ("xesam:album", Value::from("Album")),
            ("xesam:artist", Value::from(vec!["Alice", "Bob"])),
            ("xesam:artUrl", Value::from("file:///art.png")),
            ("mpris:length", Value::from(180_000_000_u64)),
            ("mpris:trackid", Value::from(op)),
        ]);

        let (title, artists, album, art_url, length_us, track_id) = parse_metadata(raw);
        assert_eq!(title, "Song");
        assert_eq!(artists, "Alice, Bob");
        assert_eq!(album, "Album");
        assert_eq!(art_url, "file:///art.png");
        assert_eq!(length_us, 180_000_000);
        assert_eq!(track_id.as_deref(), Some("/track/42"));
    }

    #[test]
    fn metadata_empty_dict_is_all_defaults() {
        let raw = metadata(vec![]);
        assert_eq!(
            parse_metadata(raw),
            (
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                0,
                None
            )
        );
    }

    #[test]
    fn metadata_non_map_value_is_all_defaults() {
        // A garbage `Metadata` value (an integer, not an `a{sv}`) must default,
        // not panic.
        assert_eq!(
            parse_metadata(owned(42_i32)),
            (
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                0,
                None
            )
        );
    }

    #[test]
    fn metadata_partial_dict_defaults_missing_fields() {
        // Only a title present, and a negative i64 length: title survives, the
        // rest default, length saturates to 0 — no panic on the mixed bag.
        let raw = metadata(vec![
            ("xesam:title", Value::from("Only Title")),
            ("mpris:length", Value::from(-1_i64)),
        ]);
        let (title, artists, album, art_url, length_us, track_id) = parse_metadata(raw);
        assert_eq!(title, "Only Title");
        assert_eq!(artists, "");
        assert_eq!(album, "");
        assert_eq!(art_url, "");
        assert_eq!(length_us, 0);
        assert_eq!(track_id, None);
    }

    #[test]
    fn metadata_bare_string_artist_survives_end_to_end() {
        // The widening has to reach the field the service actually reads, not
        // just `parse_artist_array` in isolation.
        let raw = metadata(vec![
            ("xesam:title", Value::from("Song")),
            ("xesam:artist", Value::from("Lone Artist")),
        ]);
        let (_, artists, ..) = parse_metadata(raw);
        assert_eq!(artists, "Lone Artist");
    }
}
