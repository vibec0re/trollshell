//! Golden byte fixtures for the plugin wire format (#450).
//!
//! `tests/proto.rs` is a strong compat suite, but every "old" byte sequence it
//! exercises is *regenerated at test time* with whatever `rmp-serde` the test
//! binary happens to link. That proves internal round-trip consistency, but it
//! can't catch a `rmp-serde`/`serde` upgrade that shifts encoding behavior:
//! both the "old" and "new" sides of those tests re-encode with the new
//! library, so the whole suite stays green while every already-deployed
//! plugin binary (built against the old library) silently breaks.
//!
//! This suite closes that gap with bytes that are **committed to git**, not
//! computed by the test run:
//!
//! - [`golden_bytes_are_pinned`] walks a table of representative messages
//!   (a `Register` handshake, a `Render` carrying the full [`Node`]
//!   vocabulary incl. [`Node::Pixels`], every [`Effect`] variant, every
//!   [`StateKey`]/[`Capability`], and the full [`HostMsg`] push set) and
//!   checks, for each entry, that:
//!   1. `encode()` of the current Rust value is byte-identical to the
//!      committed fixture (**encode stability** — catches an encoder change);
//!   2. decoding the fixture's *committed bytes* reproduces the same value
//!      (**decode stability** — catches a decoder change).
//! - [`frozen_shutdown_v1_hardcoded_bytes_decode_and_reencode`] goes one step
//!   further for a single message: the bytes live as a literal hex string
//!   *in this file*, not in a fixture loaded from disk, so it can never drift
//!   even if `tests/fixtures/` were accidentally regenerated or deleted.
//!
//! # Regenerating the fixtures
//!
//! Only do this for an **intentional** wire change (a new field/variant, or a
//! deliberate `PROTO_VERSION` bump) — never to make a red CI run pass:
//!
//! ```sh
//! cargo test -p hytte-plugin-proto --test golden -- --ignored --nocapture regenerate_golden_fixtures
//! ```
//!
//! It (over)writes every file under `tests/fixtures/` from the table below,
//! then deliberately fails so a regeneration run can never pass silently.
//! Inspect `git diff crates/hytte-plugin-proto/tests/fixtures/` before
//! committing — a diff you can't explain from the source change is exactly
//! the wire break this suite exists to catch.

use hytte_plugin_proto::{
    AudioAction, AudioSpectrum, Capability, ClockState, ConsentDecision, DatasourceError,
    DatasourceOutcome, Dir, Effect, EffectOutcome, EventKind, HostMsg, LogLevel, Manifest,
    MediaAction, Mount, NiriAction, Node, NowPlaying, PROTO_VERSION, Page, PluginMsg,
    ProvidedDatasource, SPECTRUM_BINS, StateKey, StateSnapshot, UpcomingEvent, decode, encode,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::fmt::Debug;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

// ── hex <-> bytes (no extra dependency — this crate takes none in `tests/`) ─

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
    assert!(
        s.len().is_multiple_of(2),
        "fixture hex must have an even number of digits, got {}",
        s.len()
    );
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("fixture contains only hex digits"))
        .collect()
}

// ── fixture I/O ──────────────────────────────────────────────────────────────

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn fixture_path(name: &str) -> PathBuf {
    fixtures_dir().join(format!("{name}.hex"))
}

fn read_fixture(name: &str) -> String {
    let path = fixture_path(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "missing golden fixture {} ({e}) — run `cargo test -p hytte-plugin-proto --test golden -- --ignored --nocapture regenerate_golden_fixtures` and commit the result",
            path.display()
        )
    })
}

// ── the message table ────────────────────────────────────────────────────────
//
// A trait object per entry lets one loop drive every message type through the
// same encode/decode/compare logic despite `PluginMsg`, `HostMsg`, `Manifest`,
// etc. all being distinct types.

trait Golden: Debug {
    fn frame(&self) -> Vec<u8>;
    fn decodes_from(&self, frame: &[u8]) -> bool;
}

impl<T> Golden for T
where
    T: Serialize + DeserializeOwned + PartialEq + Debug,
{
    fn frame(&self) -> Vec<u8> {
        encode(self)
    }

    fn decodes_from(&self, frame: &[u8]) -> bool {
        decode::<T>(frame).is_ok_and(|decoded| &decoded == self)
    }
}

/// A manifest exercising every [`StateKey`] and every [`Capability`] (the
/// full subscription/capability vocabulary), a non-default [`Mount`], and a
/// set placement `order` — the "manifest with capabilities/subscribes" entry.
fn full_manifest() -> Manifest {
    Manifest {
        id: "vibectl".into(),
        proto: PROTO_VERSION,
        subscribes: vec![
            StateKey::Clock,
            StateKey::SlotVisible,
            StateKey::Accent,
            StateKey::AudioSpectrum,
            StateKey::CalendarUpcoming,
            StateKey::SessionLocked,
            StateKey::NowPlaying,
        ],
        capabilities: vec![
            Capability::OpenPage,
            Capability::Niri,
            Capability::Media,
            Capability::Audio,
            Capability::RunCommand,
            Capability::RaiseOsd,
            Capability::Notify,
            Capability::Consent,
            Capability::Calendar,
            Capability::SessionState,
            Capability::NowPlaying,
            Capability::DatasourceQuery,
            Capability::DatasourceProvider,
        ],
        mount: Mount::SidebarLead,
        order: Some(-5),
        provides: vec![
            ProvidedDatasource::new("departures", vec!["next".into()]),
            ProvidedDatasource::new("weather", vec!["current".into()]),
        ],
    }
}

/// The `ListBox`/`Row` pair, nested inside [`node_tree`] below.
fn list_tree() -> Node {
    Node::ListBox {
        id: Some("list".into()),
        classes: vec!["ts-list".into()],
        children: vec![Node::Row {
            id: Some("row-0".into()),
            classes: vec!["ts-row".into()],
            children: vec![Node::Text {
                id: Some("plat".into()),
                text: "spor 2".into(),
                max_width_chars: None,
                ellipsize: false,
                classes: vec![],
            }],
        }],
    }
}

/// The `Expander` (#333) subtree, nested inside [`node_tree`] below.
fn expander_tree() -> Node {
    Node::Expander {
        id: "living-room".into(),
        header: Box::new(Node::Label {
            id: None,
            text: "Living Room".into(),
            classes: vec!["heading".into()],
        }),
        children: vec![Node::Row {
            id: Some("lamp".into()),
            classes: vec![],
            children: vec![Node::Label {
                id: None,
                text: "Lamp".into(),
                classes: vec![],
            }],
        }],
        expanded: true,
        classes: vec!["boxed-list".into()],
    }
}

/// A tree touching every [`Node`] variant, including [`Node::Pixels`] with a
/// non-default `scale` — the "small Node tree incl. `Node::Pixels`" entry.
fn node_tree() -> Node {
    Node::Box {
        id: Some("root".into()),
        dir: Dir::Vertical,
        spacing: 6,
        scroll: true,
        classes: vec!["ts-card".into()],
        children: vec![
            Node::Label {
                id: None,
                text: "hi".into(),
                classes: vec![],
            },
            Node::Text {
                id: Some("dest".into()),
                text: "a long ellipsized destination".into(),
                max_width_chars: Some(24),
                ellipsize: true,
                classes: vec!["ts-dest".into()],
            },
            Node::Icon {
                id: Some("ico".into()),
                name: "weather-clear-symbolic".into(),
                classes: vec!["ts-icon".into()],
            },
            Node::Pixels {
                id: Some("lcd".into()),
                width: 2,
                height: 2,
                data: vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
                scale: 2,
                classes: vec!["ts-lcd".into()],
            },
            Node::Button {
                id: "go".into(),
                classes: vec!["ts-btn".into()],
                child: Box::new(Node::Label {
                    id: None,
                    text: "Go".into(),
                    classes: vec![],
                }),
            },
            Node::Progress {
                id: None,
                fraction: 0.42,
                classes: vec![],
            },
            Node::Slider {
                id: "brightness".into(),
                min: 0.0,
                max: 1.0,
                value: 0.7,
                step: 0.05,
                enabled: false,
                classes: vec!["ts-slider".into()],
            },
            Node::Revealer {
                id: None,
                open: false,
                child: Box::new(Node::Separator { classes: vec![] }),
            },
            Node::Separator {
                classes: vec!["ts-sep".into()],
            },
            Node::Spacer,
            list_tree(),
            expander_tree(),
            Node::Entry {
                id: "term-input".into(),
                text: String::new(),
                placeholder: "type a command…".into(),
                classes: vec!["monospace".into()],
            },
        ],
    }
}

/// A small, distinct tree for `Render`'s optional `panel` field (#349 PR2).
fn panel_tree() -> Node {
    Node::Label {
        id: Some("panel-lbl".into()),
        text: "panel body".into(),
        classes: vec![],
    }
}

/// Every [`Effect`] variant, including [`Effect::Notify`], [`Effect::RaiseOsd`],
/// and [`Effect::RequestConsent`] (#487) — the "each Effect" entry.
fn effect_table() -> Vec<Effect> {
    vec![
        Effect::OpenPage(Page::PluginSelf),
        Effect::Niri(NiriAction::FocusWorkspace { id: 3 }),
        Effect::Niri(NiriAction::FocusWindow { id: 99 }),
        Effect::Media(MediaAction::PlayPause),
        Effect::Media(MediaAction::Next),
        Effect::Media(MediaAction::Previous),
        Effect::Audio(AudioAction::SetVolume(0.5)),
        Effect::Audio(AudioAction::ToggleMute),
        Effect::RunCommand {
            id: 7,
            argv: vec!["vibectl".into(), "status".into()],
        },
        Effect::RaiseOsd {
            title: "Leave now".into(),
            body: "S9 · Spandau · 16:05".into(),
            icon: Some("appointment-soon-symbolic".into()),
        },
        Effect::RaiseOsd {
            title: "Leave soon".into(),
            body: "S9 · Spandau · 16:05".into(),
            icon: None,
        },
        Effect::Notify {
            summary: "Timer done".into(),
            body: "25:00 timer finished".into(),
        },
        Effect::RequestConsent {
            request_id: 7,
            agent: "claude".into(),
            datasource: "departures".into(),
            scope: "*".into(),
            detail: "next S-Bahn departures".into(),
        },
        Effect::DatasourceQuery {
            request_id: 11,
            provider: "departures".into(),
            scope: "next".into(),
            params: r#"{"limit":5}"#.into(),
        },
        Effect::DatasourceResult {
            request_id: 12,
            outcome: DatasourceOutcome::Ready(r#"[{"line":"S9","direction":"Spandau"}]"#.into()),
        },
        Effect::DatasourceResult {
            request_id: 13,
            outcome: DatasourceOutcome::Failed {
                error: DatasourceError::Provider,
                message: "fetch failed".into(),
            },
        },
    ]
}

/// Every [`LogLevel`] plus [`PluginMsg::Pong`] — the remaining `PluginMsg`
/// variants not covered by `Register`/`Render` below.
fn plugin_control_msgs() -> Vec<PluginMsg> {
    vec![
        PluginMsg::Log {
            level: LogLevel::Error,
            msg: "disk full".into(),
        },
        PluginMsg::Log {
            level: LogLevel::Warn,
            msg: "heads up".into(),
        },
        PluginMsg::Log {
            level: LogLevel::Info,
            msg: "started".into(),
        },
        PluginMsg::Log {
            level: LogLevel::Debug,
            msg: "tick".into(),
        },
        PluginMsg::Log {
            level: LogLevel::Trace,
            msg: "poll".into(),
        },
        PluginMsg::Pong { seq: 42 },
    ]
}

/// Every `HostMsg` variant, including the opt-in-gated
/// [`HostMsg::Accent`]/[`HostMsg::AudioSpectrum`] pushes (both the resolved
/// and unresolved `Accent` case), the #487 [`HostMsg::ConsentDecision`] push,
/// the #484/#528 domain pushes ([`HostMsg::CalendarUpcoming`] populated and
/// empty, [`HostMsg::SessionLocked`] both states, [`HostMsg::NowPlaying`] playing
/// and idle), the #509 datasource pushes, and every [`EventKind`] — the "`StateKey`
/// variants incl Accent/AudioSpectrum" entry lives here as the pushes those
/// subscriptions gate.
// A flat data table of every variant; splitting it into helpers gains nothing.
#[allow(clippy::too_many_lines)]
fn host_msgs() -> Vec<HostMsg> {
    let mut bins = [0.0_f32; SPECTRUM_BINS];
    for (i, b) in bins.iter_mut().enumerate() {
        #[allow(clippy::cast_precision_loss)]
        {
            *b = i as f32 / SPECTRUM_BINS as f32;
        }
    }
    vec![
        HostMsg::StateSnapshot {
            snapshot: StateSnapshot {
                clock: Some(ClockState {
                    iso: "2026-07-11T15:49:00+02:00".into(),
                    unix: 1_752_248_940,
                }),
            },
        },
        HostMsg::StateSnapshot {
            snapshot: StateSnapshot::default(),
        },
        HostMsg::Event {
            node: "go".into(),
            kind: EventKind::Click,
        },
        HostMsg::Event {
            node: "scroller".into(),
            kind: EventKind::Scroll { dx: 0.0, dy: -1.5 },
        },
        HostMsg::Event {
            node: "brightness".into(),
            kind: EventKind::ValueChanged { value: 0.62 },
        },
        HostMsg::Event {
            node: "term-input".into(),
            kind: EventKind::Submitted {
                text: "caw --help".into(),
            },
        },
        HostMsg::EffectResult {
            id: 7,
            outcome: EffectOutcome {
                ok: true,
                output: Some("running".into()),
            },
        },
        HostMsg::EffectResult {
            id: 8,
            outcome: EffectOutcome {
                ok: false,
                output: None,
            },
        },
        HostMsg::SlotVisibility { visible: true },
        HostMsg::SlotVisibility { visible: false },
        HostMsg::Accent {
            color: Some([0x35, 0x84, 0xe4, 0xff]),
        },
        HostMsg::Accent { color: None },
        HostMsg::AudioSpectrum {
            spectrum: AudioSpectrum { peak: 0.75, bins },
        },
        HostMsg::ConsentDecision {
            request_id: 7,
            decision: ConsentDecision::AllowAlways,
        },
        HostMsg::ConsentDecision {
            request_id: 8,
            decision: ConsentDecision::Deny,
        },
        HostMsg::CalendarUpcoming {
            events: vec![
                UpcomingEvent {
                    start_unix: 1_752_248_940,
                    end_unix: 1_752_252_540,
                    title: "standup".into(),
                    calendar: "Work".into(),
                },
                UpcomingEvent {
                    start_unix: 1_752_260_000,
                    end_unix: 1_752_263_600,
                    title: "the thing".into(),
                    calendar: "Personal".into(),
                },
            ],
        },
        HostMsg::CalendarUpcoming { events: Vec::new() },
        HostMsg::SessionLocked { locked: true },
        HostMsg::SessionLocked { locked: false },
        HostMsg::NowPlaying {
            now_playing: NowPlaying {
                title: "Chrome Rain".into(),
                artist: "Choom".into(),
                playing: true,
            },
        },
        HostMsg::NowPlaying {
            now_playing: NowPlaying::default(),
        },
        HostMsg::DatasourceQuery {
            request_id: 44,
            datasource: "departures".into(),
            scope: "next".into(),
            params: r#"{"limit":5}"#.into(),
        },
        HostMsg::DatasourceResult {
            request_id: 44,
            outcome: DatasourceOutcome::Ready(r#"[{"line":"S9","direction":"Spandau"}]"#.into()),
        },
        HostMsg::DatasourceResult {
            request_id: 45,
            outcome: DatasourceOutcome::Failed {
                error: DatasourceError::Timeout,
                message: "provider did not answer".into(),
            },
        },
        HostMsg::Ping { seq: 1 },
        HostMsg::Shutdown,
    ]
}

fn golden_table() -> Vec<(&'static str, Box<dyn Golden>)> {
    vec![
        ("manifest_full_v1", Box::new(full_manifest())),
        (
            "plugin_register_v1",
            Box::new(PluginMsg::Register {
                manifest: full_manifest(),
            }),
        ),
        (
            "plugin_render_v1",
            Box::new(PluginMsg::Render {
                tree: node_tree(),
                panel: Some(panel_tree()),
                effects: effect_table(),
            }),
        ),
        ("plugin_control_msgs_v1", Box::new(plugin_control_msgs())),
        ("host_msgs_v1", Box::new(host_msgs())),
    ]
}

// ── the pinning test ─────────────────────────────────────────────────────────

#[test]
fn golden_bytes_are_pinned() {
    assert_eq!(
        PROTO_VERSION, 1,
        "fixtures are named _v1; a PROTO_VERSION bump needs new _v2 fixtures alongside them (see module docs)"
    );

    for (name, msg) in golden_table() {
        let want_hex = read_fixture(name);
        let want_bytes = from_hex(&want_hex);

        // Encode stability: today's encoder must still produce exactly the
        // committed bytes for this value.
        let got = msg.frame();
        assert_eq!(
            to_hex(&got),
            want_hex.trim(),
            "encode({name}) drifted from the committed golden at tests/fixtures/{name}.hex — \
             if this is an INTENTIONAL wire change, regenerate with \
             `cargo test -p hytte-plugin-proto --test golden -- --ignored --nocapture regenerate_golden_fixtures`"
        );

        // Decode stability: the committed bytes (not today's `encode()`
        // output) must still decode to the same value.
        assert!(
            msg.decodes_from(&want_bytes),
            "committed golden tests/fixtures/{name}.hex no longer decodes to the expected {msg:?}"
        );
    }
}

/// Not run by default (`cargo test` skips `#[ignore]`d tests) — an explicit,
/// intentional action for when the wire format legitimately changes. Always
/// fails on purpose so a regeneration run can never pass silently in CI or
/// slip by unnoticed; inspect the resulting `git diff` before committing.
#[test]
#[ignore = "regenerates tests/fixtures/*.hex from the current encoder — run explicitly, never in CI"]
fn regenerate_golden_fixtures() {
    std::fs::create_dir_all(fixtures_dir()).expect("create tests/fixtures/");
    for (name, msg) in golden_table() {
        let path = fixture_path(name);
        let hex = to_hex(&msg.frame());
        std::fs::write(&path, format!("{hex}\n"))
            .unwrap_or_else(|e| panic!("failed writing fixture {}: {e}", path.display()));
        println!("wrote {} ({} bytes)", path.display(), hex.len() / 2);
    }
    panic!(
        "fixtures (re)written from the current encoder — inspect `git diff tests/fixtures/`, \
         confirm the diff matches an intentional wire change, then commit. Failing on purpose \
         so this test can never silently pass."
    );
}

// ── one fixture frozen inline, independent of tests/fixtures/ ───────────────

/// The literal `MessagePack` v1 encoding of `HostMsg::Shutdown` — a whole
/// length-prefixed frame (4-byte BE length prefix + body), captured by hand
/// from a known-good `encode()` run and pasted here as a plain hex literal.
///
/// Unlike the table above (which reads its expected bytes from
/// `tests/fixtures/`), this constant lives directly in Rust source: it can
/// never be affected by a fixture file going missing, being accidentally
/// regenerated, or a `tests/fixtures/` directory-wide mistake. It is the
/// most literal form of the "old-bytes must still decode" regression guard —
/// if an `rmp-serde`/`serde` upgrade ever changes how a bare unit enum
/// variant encodes, this is the assertion that catches it.
///
/// Do not "fix" this constant to match a new `encode()` output — if it ever
/// legitimately needs to change (a `PROTO_VERSION` bump), replace it
/// deliberately and explain why in the commit, the same as any other golden.
const FROZEN_SHUTDOWN_V1_HEX: &str = "00000009a853687574646f776e";

#[test]
fn frozen_shutdown_v1_hardcoded_bytes_decode_and_reencode() {
    let frozen = from_hex(FROZEN_SHUTDOWN_V1_HEX);
    let expected = HostMsg::Shutdown;

    let decoded: HostMsg = decode(&frozen).expect("frozen v1 Shutdown frame decodes");
    assert_eq!(
        decoded, expected,
        "frozen bytes must decode to HostMsg::Shutdown"
    );

    assert_eq!(
        encode(&expected),
        frozen,
        "the current encoder must still reproduce the frozen v1 Shutdown bytes byte-for-byte"
    );
}
