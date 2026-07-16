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
        order: None,
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
            Node::ListBox {
                id: Some("list".into()),
                classes: vec!["ts-list".into()],
                children: vec![Node::Row {
                    id: Some("row-0".into()),
                    classes: vec!["ts-row".into()],
                    children: vec![
                        // Ellipsizing destination + a Spacer + the value: the
                        // real departures/weather row shape (#295/#296).
                        Node::Text {
                            id: None,
                            text: "a long ellipsized destination name".into(),
                            max_width_chars: Some(24),
                            ellipsize: true,
                            classes: vec!["ts-dest".into()],
                        },
                        Node::Spacer,
                        Node::Text {
                            id: Some("plat".into()),
                            text: "spor 2".into(),
                            max_width_chars: None,
                            ellipsize: false,
                            classes: vec![],
                        },
                    ],
                }],
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
                enabled: true,
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
            Node::Entry {
                id: "term-input".into(),
                text: String::new(),
                placeholder: "type a command…".into(),
                classes: vec!["monospace".into()],
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
        Effect::RaiseOsd {
            title: "Leave now".into(),
            body: "S9 · Spandau · 16:05".into(),
            icon: Some("appointment-soon-symbolic".into()),
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
    round_trip_host(&HostMsg::Event {
        node: "brightness".into(),
        kind: EventKind::ValueChanged { value: 0.62 },
    });
    round_trip_host(&HostMsg::EffectResult {
        id: 7,
        outcome: EffectOutcome {
            ok: true,
            output: Some("running".into()),
        },
    });
    round_trip_host(&HostMsg::SlotVisibility { visible: true });
    round_trip_host(&HostMsg::SlotVisibility { visible: false });
    round_trip_host(&HostMsg::Ping { seq: 1 });
    round_trip_host(&HostMsg::Shutdown);
}

#[test]
fn full_node_tree_round_trips() {
    let tree = sample_tree();
    let back: Node = decode(&encode(&tree)).expect("decode Node");
    assert_eq!(tree, back);
}

// ── Spacer + Text ellipsize (#297) ───────────────────────────────────────────

#[test]
fn spacer_round_trips() {
    let node = Node::Spacer;
    let back: Node = decode(&encode(&node)).expect("decode Spacer");
    assert_eq!(node, back);
}

#[test]
fn spacer_is_name_tagged() {
    // `Spacer` is a fieldless (unit) variant, so external tagging emits it as the
    // bare variant *name* — the name is what keeps appending a variant additive
    // (older code skips an unknown tag). Prove the name rides the wire.
    let body = encode_body(&Node::Spacer);
    assert!(contains(&body, b"Spacer"), "variant name 'Spacer' present");
}

#[test]
fn expander_round_trips() {
    // The #333 collapsible-row variant: nested header + body children + the
    // `expanded` mutable prop must all survive a MessagePack round-trip.
    for expanded in [true, false] {
        let node = Node::Expander {
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
            expanded,
            classes: vec!["boxed-list".into()],
        };
        let back: Node = decode(&encode(&node)).expect("decode Expander");
        assert_eq!(node, back, "expanded={expanded} round-trips");
    }
}

#[test]
fn expander_is_name_tagged() {
    // Appended name-tagged variant → the name rides the wire (what keeps older
    // code skipping it, and keeps PROTO_VERSION at 1).
    let body = encode_body(&Node::Expander {
        id: "e".into(),
        header: Box::new(Node::Spacer),
        children: vec![],
        expanded: false,
        classes: vec![],
    });
    assert!(
        contains(&body, b"Expander"),
        "variant name 'Expander' present"
    );
}

#[test]
fn text_ellipsize_round_trips() {
    for ellipsize in [true, false] {
        let node = Node::Text {
            id: Some("dest".into()),
            text: "long destination".into(),
            max_width_chars: Some(22),
            ellipsize,
            classes: vec!["ts-dest".into()],
        };
        let back: Node = decode(&encode(&node)).expect("decode Text");
        assert_eq!(node, back, "ellipsize={ellipsize} round-trips");
    }
}

#[test]
fn text_without_ellipsize_decodes_old_frame_compat() {
    // A `Text` frame built before #297 has no `ellipsize` key. The current
    // decoder must still accept it, defaulting `ellipsize` to `false`
    // (`#[serde(default)]` + named-map encoding) — the backward-compat guarantee
    // that keeps already-deployed plugins (departures/weather) rendering. Modeled
    // as an externally-tagged enum mirroring the pre-#297 field set, so it
    // serializes as `{"Text": { id, text, classes }}` exactly like an old plugin.
    #[derive(serde::Serialize)]
    enum NodeOld {
        Text {
            id: Option<String>,
            text: String,
            classes: Vec<String>,
        },
    }

    let old = NodeOld::Text {
        id: Some("dest".into()),
        text: "an old destination".into(),
        classes: vec!["ts-dest".into()],
    };
    let body = encode_body(&old);
    assert!(
        !contains(&body, b"ellipsize"),
        "an old frame carries no ellipsize key"
    );
    let decoded: Node = decode_body(&body).expect("decode pre-#297 Text frame");
    assert_eq!(
        decoded,
        Node::Text {
            id: Some("dest".into()),
            text: "an old destination".into(),
            // Both #274's `max_width_chars` and #297's `ellipsize` default when
            // absent from an old frame.
            max_width_chars: None,
            ellipsize: false,
            classes: vec!["ts-dest".into()],
        },
        "absent ellipsize (and max_width_chars) default",
    );
}

// ── Slider node + ValueChanged event (#315) ──────────────────────────────────

#[test]
fn slider_node_round_trips() {
    let node = Node::Slider {
        id: "brightness".into(),
        min: 0.0,
        max: 1.0,
        value: 0.42,
        step: 0.05,
        // Exercise the non-default so the field is proven to round-trip.
        enabled: false,
        classes: vec!["ts-slider".into(), "osd".into()],
    };
    let back: Node = decode(&encode(&node)).expect("decode Slider");
    assert_eq!(node, back);
}

#[test]
fn slider_is_name_tagged_and_additive() {
    // `Slider` is a brand-new, externally-tagged variant, so it rides the wire
    // as its bare variant *name* — the property that makes appending it additive
    // (`PROTO_VERSION` stays 1): an older decoder skips an unknown tag rather
    // than mis-decoding an existing variant, and every pre-#315 frame (which
    // can't carry a `Slider`) decodes byte-for-byte unchanged.
    let body = encode_body(&Node::Slider {
        id: "vol".into(),
        min: 0.0,
        max: 100.0,
        value: 30.0,
        step: 1.0,
        enabled: true,
        classes: vec![],
    });
    assert!(contains(&body, b"Slider"), "variant name 'Slider' present");
}

#[test]
fn slider_without_enabled_decodes_old_frame_compat() {
    // A `Slider` frame built before the `enabled` field (an older plugin SDK)
    // carries no `enabled` key. The current decoder must still accept it,
    // defaulting `enabled` to `true` (`#[serde(default)]` + named-map encoding) —
    // so an already-deployed plugin's slider stays interactive, never silently
    // greyed. Modeled as an externally-tagged enum mirroring the pre-field set,
    // so it serializes as `{"Slider": { id, min, max, value, step, classes }}`
    // exactly like an old plugin, and `PROTO_VERSION` stays 1.
    #[derive(serde::Serialize)]
    enum NodeOld {
        Slider {
            id: String,
            min: f64,
            max: f64,
            value: f64,
            step: f64,
            classes: Vec<String>,
        },
    }

    let old = NodeOld::Slider {
        id: "brightness".into(),
        min: 0.0,
        max: 100.0,
        value: 60.0,
        step: 5.0,
        classes: vec!["ts-slider".into()],
    };
    let body = encode_body(&old);
    assert!(
        !contains(&body, b"enabled"),
        "an old frame carries no enabled key"
    );
    let decoded: Node = decode_body(&body).expect("decode pre-enabled Slider frame");
    assert_eq!(
        decoded,
        Node::Slider {
            id: "brightness".into(),
            min: 0.0,
            max: 100.0,
            value: 60.0,
            step: 5.0,
            // Absent `enabled` defaults to an interactive slider.
            enabled: true,
            classes: vec!["ts-slider".into()],
        },
        "absent enabled defaults to true (interactive)",
    );
}

// ── Entry node + Submitted event (#357) ──────────────────────────────────────

#[test]
fn entry_node_round_trips() {
    let node = Node::Entry {
        id: "term-input".into(),
        text: "ls -la".into(),
        placeholder: "type a command…".into(),
        classes: vec!["monospace".into()],
    };
    let back: Node = decode(&encode(&node)).expect("decode Entry");
    assert_eq!(node, back);
}

#[test]
fn entry_is_name_tagged_and_additive() {
    // `Entry` is a brand-new, externally-tagged variant, so it rides the wire
    // as its bare variant *name* — the property that makes appending it additive
    // (`PROTO_VERSION` stays 1): an older decoder skips an unknown tag rather
    // than mis-decoding an existing variant, and every pre-#357 frame (which
    // can't carry an `Entry`) decodes byte-for-byte unchanged.
    assert_eq!(
        PROTO_VERSION, 1,
        "appending a variant must not bump the proto"
    );
    let body = encode_body(&Node::Entry {
        id: "in".into(),
        text: String::new(),
        placeholder: String::new(),
        classes: vec![],
    });
    assert!(contains(&body, b"Entry"), "variant name 'Entry' present");
}

#[test]
fn submitted_event_round_trips() {
    // The paired host→plugin event. Like `ValueChanged` (#315), it is opt-in
    // *by vocabulary* (#305): the host only addresses an `Event` at a node the
    // plugin itself rendered, and a plugin built against a pre-#357 proto can't
    // emit a `Node::Entry` — so it can never be the target of a `Submitted` and
    // never has to decode the unknown variant. On the wire it is just another
    // name-tagged `EventKind` variant carrying its `text`.
    let msg = HostMsg::Event {
        node: "term-input".into(),
        kind: EventKind::Submitted {
            text: "caw --help".into(),
        },
    };
    let back: HostMsg = decode(&encode(&msg)).expect("decode Submitted event");
    assert_eq!(msg, back);
    assert!(
        contains(
            &encode_body(&EventKind::Submitted { text: "x".into() }),
            b"Submitted"
        ),
        "variant name 'Submitted' rides the wire (appending it is additive)",
    );
}

#[test]
fn value_changed_event_round_trips() {
    // The paired host→plugin event. It only reaches a plugin that rendered a
    // `Slider` (the structural opt-in of #305/#315), but on the wire it is just
    // another name-tagged `EventKind` variant carrying its `value`.
    let msg = HostMsg::Event {
        node: "brightness".into(),
        kind: EventKind::ValueChanged { value: 0.375 },
    };
    let back: HostMsg = decode(&encode(&msg)).expect("decode ValueChanged event");
    assert_eq!(msg, back);
    assert!(
        contains(
            &encode_body(&EventKind::ValueChanged { value: 0.5 }),
            b"ValueChanged"
        ),
        "variant name 'ValueChanged' rides the wire (appending it is additive)",
    );
}

// ── RaiseOsd effect (#236) ───────────────────────────────────────────────────

#[test]
fn raise_osd_is_name_tagged_and_additive() {
    // `RaiseOsd` is a brand-new, externally-tagged `Effect` variant, so it rides
    // the wire as its bare variant *name* — the property that makes appending it
    // additive: an older decoder skips an unknown tag rather than mis-decoding an
    // existing variant, and every pre-#236 frame (which can't carry a `RaiseOsd`)
    // decodes byte-for-byte unchanged. So `PROTO_VERSION` stays put.
    assert_eq!(
        PROTO_VERSION, 1,
        "appending a variant must not bump the proto"
    );

    let body = encode_body(&Effect::RaiseOsd {
        title: "Leave now".into(),
        body: "S9 · Spandau · 16:05".into(),
        icon: Some("appointment-soon-symbolic".into()),
    });
    assert!(
        contains(&body, b"RaiseOsd"),
        "variant name 'RaiseOsd' present"
    );

    // Round-trips both the named-icon and the host-default (`None`) icon path.
    for icon in [Some("appointment-soon-symbolic".to_owned()), None] {
        let effect = Effect::RaiseOsd {
            title: "Leave soon".into(),
            body: "S9 · Spandau · 16:05".into(),
            icon,
        };
        let back: Effect = decode(&encode(&effect)).expect("decode RaiseOsd");
        assert_eq!(effect, back);
    }
}

// ── Pixels node ──────────────────────────────────────────────────────────────

#[test]
fn pixels_node_round_trips() {
    // Exercise a non-default `scale` so the #358 field is proven to round-trip.
    let node = Node::Pixels {
        id: Some("lcd".into()),
        width: 4,
        height: 2,
        data: (0u8..32).collect(), // 4*2*4
        scale: 3,
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
        scale: 1,
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
        scale: 1,
        classes: vec![],
    };
    let back: Node = decode(&encode(&node)).expect("permissive decode of a bad-size Pixels");
    assert_eq!(node, back);
}

#[test]
fn pixels_without_scale_decodes_old_frame_compat() {
    // A `Pixels` frame built before #358 has no `scale` key. The current
    // decoder must still accept it, defaulting `scale` to `1`
    // (`#[serde(default = …)]` + named-map encoding) — the backward-compat
    // guarantee that keeps an already-deployed plugin's LCD at its pre-#358
    // 1× size. Modeled as an externally-tagged enum mirroring the pre-#358
    // field set, so it serializes as `{"Pixels": { id, width, height, data,
    // classes }}` exactly like an old plugin, and `PROTO_VERSION` stays 1.
    #[derive(serde::Serialize)]
    enum NodeOld {
        Pixels {
            id: Option<String>,
            width: u32,
            height: u32,
            #[serde(with = "serde_bytes")]
            data: Vec<u8>,
            classes: Vec<String>,
        },
    }

    let old = NodeOld::Pixels {
        id: Some("lcd".into()),
        width: 1,
        height: 1,
        data: vec![10, 20, 30, 255],
        classes: vec!["ts-lcd".into()],
    };
    let body = encode_body(&old);
    assert!(
        !contains(&body, b"scale"),
        "an old frame carries no scale key"
    );
    let decoded: Node = decode_body(&body).expect("decode pre-#358 Pixels frame");
    assert_eq!(
        decoded,
        Node::Pixels {
            id: Some("lcd".into()),
            width: 1,
            height: 1,
            data: vec![10, 20, 30, 255],
            // Absent `scale` defaults to the buffer's natural 1× size.
            scale: 1,
            classes: vec!["ts-lcd".into()],
        },
        "absent scale defaults to 1",
    );
}

#[test]
fn pixels_with_scale_is_skipped_by_an_old_decoder_forward_compat() {
    // The reverse direction the issue flags explicitly: a NEW plugin frame
    // carrying the `scale` key hits an OLD host built before #358. The old
    // decoder must *skip* the unknown field (named-map encoding + serde's
    // default ignore-unknown-fields) rather than erroring and killing the
    // session — so `scale` can ship without gating emission on the Register
    // handshake. Modeled with a pre-#358 replica of the variant deriving
    // `Deserialize`, fed the real current encoding.
    #[derive(Debug, PartialEq, serde::Deserialize)]
    enum NodeOld {
        Pixels {
            id: Option<String>,
            width: u32,
            height: u32,
            #[serde(with = "serde_bytes")]
            data: Vec<u8>,
            classes: Vec<String>,
        },
    }

    let new = Node::Pixels {
        id: Some("lcd".into()),
        width: 1,
        height: 1,
        data: vec![10, 20, 30, 255],
        scale: 2,
        classes: vec!["ts-lcd".into()],
    };
    let body = encode_body(&new);
    assert!(contains(&body, b"scale"), "the new frame carries scale");
    let decoded: NodeOld = decode_body(&body).expect("old decoder skips the unknown scale field");
    assert_eq!(
        decoded,
        NodeOld::Pixels {
            id: Some("lcd".into()),
            width: 1,
            height: 1,
            data: vec![10, 20, 30, 255],
            classes: vec!["ts-lcd".into()],
        },
        "an old host renders the same buffer at its 1× default",
    );
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
fn manifest_without_order_decodes_old_plugin_compat() {
    // An older plugin (built before `order` existed) sends a Register whose
    // manifest map has NO `order` key. The current host must still decode it,
    // defaulting `order` to `None` (relies on `#[serde(default)]` + named-map
    // encoding). This is the backward-compat guarantee for the migration.
    #[derive(serde::Serialize)]
    struct ManifestNoOrder {
        id: String,
        proto: u16,
        subscribes: Vec<StateKey>,
        capabilities: Vec<Capability>,
        mount: Mount,
    }

    let old = ManifestNoOrder {
        id: "vibectl".into(),
        proto: PROTO_VERSION,
        subscribes: vec![StateKey::Clock],
        capabilities: vec![Capability::OpenPage, Capability::RunCommand],
        mount: Mount::SidebarTop,
    };

    let body = encode_body(&old);
    // A field-less-`order` manifest must NOT put `order` on the wire (so old and
    // new field-less frames are byte-identical): `skip_serializing_if` on `None`
    // guarantees the same for a modern `order: None` manifest.
    assert!(
        !contains(&body, b"order"),
        "absent order stays off the wire"
    );
    let decoded: Manifest = decode_body(&body).expect("decode old field-less manifest");
    assert_eq!(decoded, sample_manifest(), "order defaults to None");
    assert_eq!(decoded.order, None);
}

#[test]
fn manifest_with_order_round_trips() {
    let m = Manifest::new("departures", Mount::SidebarTop).with_order(-5);
    assert_eq!(m.order, Some(-5));
    let body = encode_body(&m);
    assert!(contains(&body, b"order"), "a set order rides the wire");
    let back: Manifest = decode_body(&body).expect("decode manifest with order");
    assert_eq!(back, m);
}

#[test]
fn every_mount_round_trips_incl_sidebar_lead() {
    // Every `Mount` variant survives a manifest round-trip, incl. the additive
    // `SidebarLead` (#301). `Mount` is an externally-tagged unit enum, so each
    // variant rides the wire as its bare name — appending `SidebarLead` leaves
    // every other variant's encoding untouched (PROTO_VERSION stays 1).
    for mount in [
        Mount::SidebarLead,
        Mount::SidebarTop,
        Mount::SidebarBottom,
        Mount::BarLeft,
        Mount::BarCenter,
        Mount::BarRight,
    ] {
        let m = Manifest::new("weather", mount);
        let back: Manifest = decode_body(&encode_body(&m)).expect("decode manifest");
        assert_eq!(back.mount, mount, "{mount:?} round-trips");
    }
    // The new variant rides the wire as its bare, name-tagged variant — the
    // property that makes appending it additive (older decoders skip an unknown
    // tag rather than mis-decoding an existing one).
    let body = encode_body(&Manifest::new("weather", Mount::SidebarLead));
    assert!(
        contains(&body, b"SidebarLead"),
        "the variant name 'SidebarLead' rides the wire"
    );
}

#[test]
fn state_key_subscription_round_trips_incl_slot_visible() {
    // The subscription set round-trips, incl. the additive `SlotVisible` (#305).
    // `StateKey` is an externally-tagged unit enum, so each key rides the wire as
    // its bare name — appending `SlotVisible` is additive (PROTO_VERSION stays 1).
    let mut m = Manifest::new("departures", Mount::SidebarBottom);
    m.subscribes = vec![StateKey::Clock, StateKey::SlotVisible];
    let body = encode_body(&m);
    assert!(
        contains(&body, b"SlotVisible"),
        "the key name 'SlotVisible' rides the wire"
    );
    let back: Manifest = decode_body(&body).expect("decode manifest with subscriptions");
    assert_eq!(
        back.subscribes,
        vec![StateKey::Clock, StateKey::SlotVisible]
    );
}

#[test]
fn backward_compat_missing_optional_field_defaults() {
    // An old payload predating a `#[serde(default)]` field (here: an empty
    // snapshot map) still decodes, defaulting the absent field.
    let empty = std::collections::BTreeMap::<String, i32>::new();
    let snapshot: StateSnapshot = decode_body(&encode_body(&empty)).expect("default missing field");
    assert_eq!(snapshot, StateSnapshot { clock: None });
}
