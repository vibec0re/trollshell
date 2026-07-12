//! Hermetic protocol tests: round-trips, the proto exact-match rule, the
//! framing errors, and the forward/backward schema-compat guarantees. No
//! sockets, no display — pure encode/decode.

use hytte_plugin_proto::{
    AudioAction, Capability, ClockState, Dir, Effect, EffectOutcome, EventKind, HostMsg, LogLevel,
    MAX_FRAME_LEN, Manifest, MediaAction, Mount, NiriAction, Node, PROTO_VERSION, Page, PluginMsg,
    ProtoError, StateKey, StateSnapshot, decode, decode_body, encode, encode_body,
};

// ── Fixtures ─────────────────────────────────────────────────────────────────

fn sample_manifest() -> Manifest {
    Manifest {
        id: "vibectl".into(),
        proto: PROTO_VERSION,
        subscribes: vec![StateKey::Clock],
        capabilities: vec![Capability::OpenPage, Capability::RunCommand],
        mount: Mount::SidebarTop,
    }
}

fn sample_tree() -> Node {
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
            Node::Revealer {
                id: None,
                open: false,
                child: Box::new(Node::Separator { classes: vec![] }),
            },
            Node::Separator {
                classes: vec!["ts-sep".into()],
            },
        ],
    }
}

fn sample_effects() -> Vec<Effect> {
    vec![
        Effect::OpenPage(Page::Media),
        Effect::Niri(NiriAction::FocusWorkspace { id: 3 }),
        Effect::Niri(NiriAction::FocusWindow { id: 99 }),
        Effect::Media(MediaAction::PlayPause),
        Effect::Audio(AudioAction::SetVolume(0.5)),
        Effect::Audio(AudioAction::ToggleMute),
        Effect::RunCommand {
            id: 7,
            argv: vec!["vibectl".into(), "status".into()],
        },
    ]
}

fn round_trip_plugin(msg: &PluginMsg) {
    let back: PluginMsg = decode(&encode(msg)).expect("decode PluginMsg");
    assert_eq!(*msg, back);
}

fn round_trip_host(msg: &HostMsg) {
    let back: HostMsg = decode(&encode(msg)).expect("decode HostMsg");
    assert_eq!(*msg, back);
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

// ── Round-trips ──────────────────────────────────────────────────────────────

#[test]
fn plugin_msgs_round_trip() {
    round_trip_plugin(&PluginMsg::Register {
        manifest: sample_manifest(),
    });
    round_trip_plugin(&PluginMsg::Render {
        tree: sample_tree(),
        effects: sample_effects(),
    });
    round_trip_plugin(&PluginMsg::Log {
        level: LogLevel::Warn,
        msg: "heads up".into(),
    });
    round_trip_plugin(&PluginMsg::Pong { seq: 42 });
}

#[test]
fn host_msgs_round_trip() {
    round_trip_host(&HostMsg::StateSnapshot {
        snapshot: StateSnapshot {
            clock: Some(ClockState {
                iso: "2026-07-11T15:49:00+02:00".into(),
                unix: 1_752_248_940,
            }),
        },
    });
    round_trip_host(&HostMsg::Event {
        node: "go".into(),
        kind: EventKind::Click,
    });
    round_trip_host(&HostMsg::Event {
        node: "scroller".into(),
        kind: EventKind::Scroll { dx: 0.0, dy: -1.5 },
    });
    round_trip_host(&HostMsg::EffectResult {
        id: 7,
        outcome: EffectOutcome {
            ok: true,
            output: Some("running".into()),
        },
    });
    round_trip_host(&HostMsg::Ping { seq: 1 });
    round_trip_host(&HostMsg::Shutdown);
}

#[test]
fn full_node_tree_round_trips() {
    let tree = sample_tree();
    let back: Node = decode(&encode(&tree)).expect("decode Node");
    assert_eq!(tree, back);
}

// ── Pixels node ──────────────────────────────────────────────────────────────

#[test]
fn pixels_node_round_trips() {
    let node = Node::Pixels {
        id: Some("lcd".into()),
        width: 4,
        height: 2,
        data: (0u8..32).collect(), // 4*2*4
        classes: vec!["ts-lcd".into()],
    };
    let back: Node = decode(&encode(&node)).expect("decode Pixels");
    assert_eq!(node, back);
}

#[test]
fn pixels_data_is_one_binary_blob() {
    // Baseline: a bare `Vec<u8>` WITHOUT serde_bytes serializes as a per-byte
    // int array (~2× for bytes >= 0x80) — the bloat serde_bytes exists to avoid.
    #[derive(serde::Serialize)]
    struct NaiveArray {
        data: Vec<u8>,
    }

    // Bytes >= 0x80 cost 2 bytes each as a MessagePack int array but 1 byte each
    // as a `bin` blob, so all-0xFF is the worst case for a bare `Vec<u8>`.
    let data = vec![0xFFu8; 1024];
    let naive = encode_body(&NaiveArray { data: data.clone() });

    // The real node routes `data` through serde_bytes → one MessagePack `bin`.
    let node = Node::Pixels {
        id: None,
        width: 16,
        height: 16,
        data: data.clone(),
        classes: vec![],
    };
    let body = encode_body(&node);

    assert!(
        body.len() < data.len() + 128,
        "Pixels body carries the buffer as one compact blob ({} B for a {} B buffer)",
        body.len(),
        data.len(),
    );
    assert!(
        naive.len() > body.len() * 3 / 2,
        "the int-array encoding ({} B) is far larger than the serde_bytes blob ({} B)",
        naive.len(),
        body.len(),
    );
}

#[test]
fn pixels_with_mismatched_len_still_round_trips() {
    // The proto layer is deliberately permissive: it does NOT enforce the
    // `width*height*4` invariant, so one malformed node decodes cleanly and can
    // never drop the whole connection. Enforcement lives at the host trust
    // boundary (`to_ui_node`), which degrades to rendering nothing + a warning.
    let node = Node::Pixels {
        id: None,
        width: 10,
        height: 10,
        data: vec![0, 1, 2], // 3 bytes, not 400
        classes: vec![],
    };
    let back: Node = decode(&encode(&node)).expect("permissive decode of a bad-size Pixels");
    assert_eq!(node, back);
}

// ── Proto exact-match rule ───────────────────────────────────────────────────

#[test]
fn register_proto_exact_match() {
    assert!(sample_manifest().check_proto().is_ok());

    let mut skewed = sample_manifest();
    skewed.proto = PROTO_VERSION + 1;
    assert!(matches!(
        skewed.check_proto(),
        Err(ProtoError::ProtoMismatch { ours, theirs })
            if ours == PROTO_VERSION && theirs == PROTO_VERSION + 1
    ));
}

// ── Framing errors ───────────────────────────────────────────────────────────

#[test]
fn truncated_frame_is_rejected() {
    let frame = encode(&PluginMsg::Pong { seq: 9 });
    let short = &frame[..frame.len() - 1];
    assert!(matches!(
        decode::<PluginMsg>(short),
        Err(ProtoError::FrameTruncated { .. })
    ));

    // Too short to even hold the length prefix.
    assert!(matches!(
        decode::<PluginMsg>(&[0u8, 1u8]),
        Err(ProtoError::FrameTruncated { .. })
    ));
}

#[test]
fn oversized_frame_is_rejected() {
    let declared = u32::try_from(MAX_FRAME_LEN + 1).expect("fits u32");
    let mut frame = declared.to_be_bytes().to_vec();
    frame.extend_from_slice(&[0u8, 0u8]);
    assert!(matches!(
        decode::<PluginMsg>(&frame),
        Err(ProtoError::FrameTooLarge { len }) if len == MAX_FRAME_LEN + 1
    ));
}

// ── Schema-compat guarantees ─────────────────────────────────────────────────

#[test]
fn named_map_encoding_is_pinned() {
    // `to_vec_named` must keep field names on the wire (a switch to positional
    // `to_vec` would drop them and break unknown-field skipping).
    let body = encode_body(&sample_manifest());
    assert!(contains(&body, b"proto"), "field name 'proto' present");
    assert!(
        contains(&body, b"subscribes"),
        "field name 'subscribes' present"
    );
    assert!(contains(&body, b"mount"), "field name 'mount' present");
}

#[test]
fn forward_compat_extra_field_is_skipped() {
    // A future proto adds a field; an older decoder must ignore it (relies on
    // named-map encoding + no `deny_unknown_fields`).
    #[derive(serde::Serialize)]
    struct ManifestPlus {
        id: String,
        proto: u16,
        subscribes: Vec<StateKey>,
        capabilities: Vec<Capability>,
        mount: Mount,
        extra_future_field: u32,
    }

    let future = ManifestPlus {
        id: "vibectl".into(),
        proto: PROTO_VERSION,
        subscribes: vec![StateKey::Clock],
        capabilities: vec![Capability::OpenPage, Capability::RunCommand],
        mount: Mount::SidebarTop,
        extra_future_field: 1234,
    };

    let decoded: Manifest = decode_body(&encode_body(&future)).expect("skip extra field");
    assert_eq!(decoded, sample_manifest());
}

#[test]
fn backward_compat_missing_optional_field_defaults() {
    // An old payload predating a `#[serde(default)]` field (here: an empty
    // snapshot map) still decodes, defaulting the absent field.
    let empty = std::collections::BTreeMap::<String, i32>::new();
    let snapshot: StateSnapshot = decode_body(&encode_body(&empty)).expect("default missing field");
    assert_eq!(snapshot, StateSnapshot { clock: None });
}
