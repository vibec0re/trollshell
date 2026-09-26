//! The manifest's `settings` list (#1410): its wire shape, pinned in bytes, and
//! the compat claims that let it ship without a [`VOCAB`] bump.
//!
//! Kept out of `golden.rs`/`proto.rs` on purpose. Those two files pin the
//! vocabulary counter through [`Manifest::new`] and `full_manifest`, so a
//! census bump moves a byte in every fixture they build; the manifest here
//! spells `vocab`/`vocab_max` as **literal numbers** instead, so
//! `manifest_settings_v1.hex` records exactly what this field put on the wire
//! and nothing a later generation bump would move.
//!
//! The pre-existing fixtures are the other half of the evidence: `golden.rs`
//! still passes over every one of them unchanged, which is the proof that a
//! settings-less manifest encodes byte-identically to a pre-#1410 one.
//!
//! # Regenerating the fixture
//!
//! Only for an intentional change to this field's shape:
//!
//! ```sh
//! cargo test -p hytte-plugin-proto --test settings_golden -- --ignored --nocapture regenerate_settings_fixture
//! ```
//!
//! [`VOCAB`]: hytte_plugin_proto::VOCAB

use hytte_plugin_proto::manifest::{MAX_SETTING_ENV_BYTES, Setting, SettingKind};
use hytte_plugin_proto::{
    Capability, Manifest, Mount, PROTO_VERSION, StateKey, decode, decode_body, encode, encode_body,
};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

const FIXTURE: &str = "manifest_settings_v1";

/// A manifest as a **newer** plugin would send it: one known setting, one of
/// a kind this build does not know, and one entry with no readable `env`
/// (#1415 review M2). Committed bytes, so the leniency is pinned against a
/// real encoding and not only against today's encoder.
const FUTURE_FIXTURE: &str = "manifest_settings_future_kind_v1";

fn fixture_path() -> PathBuf {
    named_fixture_path(FIXTURE)
}

fn named_fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/{name}.hex"))
}

fn to_hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

fn from_hex(s: &str) -> Vec<u8> {
    let s = s.trim();
    assert!(s.len().is_multiple_of(2), "odd hex digit count");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex digits only"))
        .collect()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// The settings every kind contributes — one of each, plus a `doc` and a
/// `default` so both optional parts are on the wire.
fn every_kind() -> Vec<Setting> {
    vec![
        Setting::path("V1BECTL_SCREENS", "Screens layout file")
            .doc("A screens.kdl; one group per room when unset.")
            .default_value("~/.config/v1bectl/screens.kdl"),
        Setting::directory("V1BECTL_CACHE", "Cache folder"),
        Setting::text("V1BECTL_SERVER", "Server address"),
        Setting::bool("V1BECTL_DEBUG", "Debug overlay"),
        Setting::int("V1BECTL_COLUMNS", "Columns", 1, 8).default_value("3"),
        Setting::choice("V1BECTL_THEME", "Theme", ["dark", "light"]),
    ]
}

/// The manifest `manifest_settings_v1.hex` records. `vocab`/`vocab_max` are
/// literals on purpose — see the module docs.
fn settings_manifest() -> Manifest {
    Manifest {
        id: "vibectl".into(),
        proto: PROTO_VERSION,
        vocab: 1,
        vocab_max: Some(6),
        subscribes: vec![StateKey::Clock],
        capabilities: vec![Capability::OpenPage],
        mount: Mount::SidebarTop,
        order: None,
        provides: Vec::new(),
        version: Some("0.3.0".into()),
        settings: every_kind(),
    }
}

// ── the golden fixture ───────────────────────────────────────────────────────

#[test]
fn the_settings_fixture_is_pinned_both_ways() {
    let committed = std::fs::read_to_string(fixture_path()).unwrap_or_else(|e| {
        panic!(
            "missing {} ({e}) — run the ignored regenerate_settings_fixture test and commit it",
            fixture_path().display()
        )
    });
    let want = from_hex(&committed);
    assert_eq!(
        to_hex(&encode(&settings_manifest())),
        committed.trim(),
        "encoder drift: the settings list no longer encodes to the committed bytes"
    );
    let decoded: Manifest = decode(&want).expect("the committed bytes decode");
    assert_eq!(decoded, settings_manifest());
}

/// Not run by default: rewrites the fixture from today's encoder, then fails
/// on purpose so a regeneration run can never pass silently.
#[test]
#[ignore = "regenerates tests/fixtures/manifest_settings_v1.hex; run by hand"]
fn regenerate_settings_fixture() {
    let hex = to_hex(&encode(&settings_manifest()));
    std::fs::write(fixture_path(), format!("{hex}\n")).expect("write fixture");
    let future = to_hex(&encode(&future_manifest()));
    std::fs::write(named_fixture_path(FUTURE_FIXTURE), format!("{future}\n"))
        .expect("write fixture");
    panic!(
        "wrote {} — inspect the diff, then commit",
        fixture_path().display()
    );
}

// ── a newer plugin's kinds (#1415 review M2) ─────────────────────────────────

/// A kind added after this build — what a plugin on a newer SDK would put in
/// its manifest.
#[derive(serde::Serialize)]
enum FutureKind {
    Text,
    Secret { reveal: bool },
}

#[derive(serde::Serialize)]
struct FutureSetting {
    #[serde(skip_serializing_if = "Option::is_none")]
    env: Option<String>,
    label: String,
    doc: String,
    kind: FutureKind,
}

/// [`Manifest`]'s own fields plus a `settings` list this build can only
/// partly read.
#[derive(serde::Serialize)]
struct FutureManifest {
    #[serde(flatten)]
    base: Manifest,
    settings: Vec<FutureSetting>,
}

fn future_manifest() -> FutureManifest {
    let setting = |env: Option<&str>, kind| FutureSetting {
        env: env.map(Into::into),
        label: "L".into(),
        doc: String::new(),
        kind,
    };
    FutureManifest {
        base: Manifest {
            id: "vibectl".into(),
            proto: PROTO_VERSION,
            vocab: 1,
            vocab_max: Some(6),
            subscribes: Vec::new(),
            capabilities: Vec::new(),
            mount: Mount::SidebarTop,
            order: None,
            provides: Vec::new(),
            version: None,
            settings: Vec::new(),
        },
        settings: vec![
            setting(Some("V1BECTL_SERVER"), FutureKind::Text),
            setting(Some("V1BECTL_TOKEN"), FutureKind::Secret { reveal: false }),
            setting(None, FutureKind::Text),
        ],
    }
}

/// One setting of a kind this build does not know costs that setting, not
/// the plugin's registration: the known one decodes as itself, the newer one
/// as [`SettingKind::Unknown`] (so the host can name it when it drops it), and
/// an entry with no readable `env` is skipped. Driven from **committed**
/// bytes, so it is a real newer encoding that is pinned.
///
/// Red before the fix: the whole `decode` failed with "unknown variant
/// `Secret`", i.e. the plugin could not register at all.
#[test]
fn a_newer_setting_kind_costs_only_its_own_setting() {
    let path = named_fixture_path(FUTURE_FIXTURE);
    let committed = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing {} ({e})", path.display()));
    assert_eq!(
        to_hex(&encode(&future_manifest())),
        committed.trim(),
        "the probe encoding drifted"
    );
    let decoded: Manifest = decode(&from_hex(&committed)).expect("the manifest still decodes");
    assert_eq!(decoded.id, "vibectl");
    assert_eq!(decoded.settings.len(), 2, "{:?}", decoded.settings);
    assert_eq!(decoded.settings[0], Setting::text("V1BECTL_SERVER", "L"));
    assert_eq!(decoded.settings[1].env, "V1BECTL_TOKEN");
    assert!(
        matches!(decoded.settings[1].kind, SettingKind::Unknown(_)),
        "{:?}",
        decoded.settings[1].kind
    );
}

/// The leniency is decode-only: an `Unknown` kind has no encoding, so a
/// shell can never forward one it read.
#[test]
fn an_unknown_kind_has_no_encoding() {
    let decoded: Manifest = decode_body(&encode_body(&future_manifest())).expect("decodes");
    assert!(rmp_serde::to_vec_named(&decoded.settings[1]).is_err());
}

// ── compat ──────────────────────────────────────────────────────────────────

#[test]
fn a_settings_less_manifest_puts_no_settings_key_on_the_wire() {
    let m = Manifest::new("clock", Mount::SidebarTop);
    assert!(m.settings.is_empty());
    let body = encode_body(&m);
    assert!(
        !contains(&body, b"settings"),
        "an empty list stays off the wire, so the frame is byte-identical to pre-#1410"
    );
    let back: Manifest = decode_body(&body).expect("decode");
    assert_eq!(back, m);
}

#[test]
fn a_manifest_with_settings_round_trips() {
    let m = Manifest::new("vibectl", Mount::SidebarTop)
        .with_setting(Setting::path("V1BECTL_SCREENS", "Screens layout file"))
        .with_setting(Setting::text("V1BECTL_SERVER", "Server address"));
    let body = encode_body(&m);
    assert!(contains(&body, b"settings"));
    let back: Manifest = decode_body(&body).expect("decode");
    assert_eq!(back, m);
    assert_eq!(
        back.settings
            .iter()
            .map(|s| s.env.as_str())
            .collect::<Vec<_>>(),
        ["V1BECTL_SCREENS", "V1BECTL_SERVER"],
        "with_setting keeps call order — the order the form draws"
    );
}

/// A host built before #1410 decodes a manifest that carries settings: the
/// unknown key is skipped whole, nested enum and all, which is why the field
/// needs no `VOCAB` bump.
#[test]
fn a_pre_settings_host_skips_the_whole_list() {
    /// `Manifest` as it stood at #887, the last field before `settings`.
    #[derive(serde::Deserialize, Debug, PartialEq)]
    struct PreSettingsManifest {
        id: String,
        proto: u16,
        #[serde(default)]
        vocab: u16,
        #[serde(default)]
        vocab_max: Option<u16>,
        subscribes: Vec<StateKey>,
        capabilities: Vec<Capability>,
        mount: Mount,
        #[serde(default)]
        order: Option<i32>,
        #[serde(default)]
        provides: Vec<hytte_plugin_proto::ProvidedDatasource>,
        #[serde(default)]
        version: Option<String>,
    }

    let old: PreSettingsManifest =
        decode_body(&encode_body(&settings_manifest())).expect("an old host decodes it");
    assert_eq!(
        old,
        PreSettingsManifest {
            id: "vibectl".into(),
            proto: PROTO_VERSION,
            vocab: 1,
            vocab_max: Some(6),
            subscribes: vec![StateKey::Clock],
            capabilities: vec![Capability::OpenPage],
            mount: Mount::SidebarTop,
            order: None,
            provides: Vec::new(),
            version: Some("0.3.0".into()),
        }
    );
}

/// A manifest from a plugin built before #1410 decodes with no settings.
#[test]
fn a_pre_settings_plugin_decodes_to_an_empty_list() {
    #[derive(serde::Serialize)]
    struct PreSettingsManifest {
        id: String,
        proto: u16,
        subscribes: Vec<StateKey>,
        capabilities: Vec<Capability>,
        mount: Mount,
    }

    let body = encode_body(&PreSettingsManifest {
        id: "pet".into(),
        proto: PROTO_VERSION,
        subscribes: Vec::new(),
        capabilities: Vec::new(),
        mount: Mount::SidebarTop,
    });
    let decoded: Manifest = decode_body(&body).expect("decode");
    assert!(decoded.settings.is_empty());
}

#[test]
fn doc_and_default_are_optional_on_the_wire() {
    #[derive(serde::Serialize)]
    struct BareSetting {
        env: String,
        label: String,
        kind: SettingKind,
    }

    let body = encode_body(&BareSetting {
        env: "FOO".into(),
        label: "Foo".into(),
        kind: SettingKind::Text,
    });
    let decoded: Setting = decode_body(&body).expect("decode");
    assert_eq!(decoded, Setting::text("FOO", "Foo"));
    assert!(decoded.doc.is_empty());
    assert_eq!(decoded.default, None);
}

// ── builders ────────────────────────────────────────────────────────────────

#[test]
fn every_constructor_picks_its_kind() {
    let kinds: Vec<SettingKind> = every_kind().into_iter().map(|s| s.kind).collect();
    assert_eq!(
        kinds,
        [
            SettingKind::Path { directory: false },
            SettingKind::Path { directory: true },
            SettingKind::Text,
            SettingKind::Bool,
            SettingKind::Int { min: 1, max: 8 },
            SettingKind::Choice {
                options: vec!["dark".into(), "light".into()],
            },
        ]
    );
}

// ── the env-name rule ───────────────────────────────────────────────────────

#[test]
fn ordinary_plugin_variables_are_settable() {
    for name in [
        "V1BECTL_SCREENS",
        "PET_NAME",
        "_UNDERSCORE_FIRST",
        "A",
        "X11",
        "RUST_LOG",
    ] {
        assert_eq!(Setting::env_refusal(name), None, "{name} must be settable");
    }
}

#[test]
fn malformed_names_are_refused() {
    for name in [
        "",
        "lower",
        "Mixed_Case",
        "1LEADING_DIGIT",
        "HAS-DASH",
        "HAS SPACE",
        "HAS=EQUALS",
        "ÄNDERUNG",
        "NUL\0",
    ] {
        assert!(
            Setting::env_refusal(name).is_some(),
            "{name:?} must be refused"
        );
    }
}

#[test]
fn reserved_names_are_refused() {
    for name in [
        "HYTTE_PLUGIN_ID",
        "HYTTE_PLUGIN_MOUNT",
        "HYTTE_ANYTHING",
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
        "XDG_RUNTIME_DIR",
        "XDG_CONFIG_HOME",
        "PATH",
        "HOME",
        "OPENROUTER_API_KEY",
        "ANTHROPIC_API_KEY",
        // #1415 review L1: these would configure `systemd-run` itself, whose
        // own environment carries a saved value.
        "SYSTEMD_LOG_LEVEL",
        "SYSTEMD_LOG_TARGET",
        "NOTIFY_SOCKET",
    ] {
        assert!(
            Setting::env_refusal(name).is_some(),
            "{name} must be refused"
        );
    }
    // Only the exact session names and the listed prefixes/suffix: a name
    // that merely contains one is a plugin's own.
    for name in [
        "MY_PATH",
        "HOMEPAGE",
        "PLUGIN_XDG_MODE",
        "API_KEY_HINT",
        "MY_SYSTEMD_UNIT",
        "NOTIFY_SOCKET_PATH",
    ] {
        assert_eq!(Setting::env_refusal(name), None, "{name} is a plugin's own");
    }
}

#[test]
fn the_length_cap_is_inclusive() {
    let at = "A".repeat(MAX_SETTING_ENV_BYTES);
    assert_eq!(Setting::env_refusal(&at), None);
    let over = "A".repeat(MAX_SETTING_ENV_BYTES + 1);
    assert!(Setting::env_refusal(&over).is_some());
}
