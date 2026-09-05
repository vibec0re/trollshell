//! Hermetic protocol tests: round-trips, the proto exact-match rule, the
//! framing errors, and the forward/backward schema-compat guarantees. No
//! sockets, no display — pure encode/decode.

use hytte_plugin_proto::{
    AudioAction, Capability, ClockState, ConsentDecision, DEFAULT_SLIDER_MAX, DEFAULT_SLIDER_MIN,
    DEFAULT_SLIDER_STEP_FRACTION, DatasourceError, DatasourceOutcome, Dir, Effect, EffectOutcome,
    EventKind, HostMsg, LedStripConfig, LedStripState, LogLevel, MAX_FRAME_LEN, Manifest,
    MediaAction, Mount, NiriAction, Node, PROTO_VERSION, Page, PluginMsg, PreemWidget, ProtoError,
    ProvidedDatasource, SliderFloats, StateKey, StateSnapshot, VOCAB, VOCAB_UNCONDITIONAL, decode,
    decode_body, encode, encode_body, sane_fraction, sane_slider_floats,
};

// ── Fixtures ─────────────────────────────────────────────────────────────────

fn sample_manifest() -> Manifest {
    Manifest {
        id: "vibectl".into(),
        proto: PROTO_VERSION,
        vocab: VOCAB_UNCONDITIONAL,
        vocab_max: Some(VOCAB),
        subscribes: vec![StateKey::Clock],
        capabilities: vec![Capability::OpenPage, Capability::RunCommand],
        mount: Mount::SidebarTop,
        order: None,
        provides: Vec::new(),
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
        Effect::Notify {
            summary: "Timer done".into(),
            body: "25:00 timer finished".into(),
        },
        Effect::DatasourceQuery {
            request_id: 3,
            provider: "departures".into(),
            scope: "next".into(),
            params: r#"{"limit":5}"#.into(),
        },
        Effect::DatasourceResult {
            request_id: 91,
            outcome: DatasourceOutcome::Ready(r#"[{"line":"S9"}]"#.into()),
        },
        Effect::DatasourceResult {
            request_id: 92,
            outcome: DatasourceOutcome::Failed {
                error: DatasourceError::Provider,
                message: "fetch failed".into(),
            },
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
        panel: None,
        effects: sample_effects(),
    });
    // A panel-bearing render is in the standard round-trip set too (#349 PR2).
    round_trip_plugin(&PluginMsg::Render {
        tree: sample_tree(),
        panel: Some(sample_tree()),
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
    round_trip_host(&HostMsg::DatasourceQuery {
        request_id: 5,
        datasource: "departures".into(),
        scope: "next".into(),
        params: r#"{"limit":3}"#.into(),
    });
    round_trip_host(&HostMsg::DatasourceResult {
        request_id: 5,
        outcome: DatasourceOutcome::Ready(r#"[{"line":"S9"}]"#.into()),
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

// ── Notify effect (#406) ─────────────────────────────────────────────────────

#[test]
fn notify_is_name_tagged_and_additive() {
    // `Notify` is a brand-new, externally-tagged `Effect` variant, so it rides
    // the wire as its bare variant *name* — the property that makes appending it
    // additive: an older decoder skips an unknown tag rather than mis-decoding an
    // existing variant, and every pre-#406 frame (which can't carry a `Notify`)
    // decodes byte-for-byte unchanged. So `PROTO_VERSION` stays put.
    assert_eq!(
        PROTO_VERSION, 1,
        "appending a variant must not bump the proto"
    );

    let body = encode_body(&Effect::Notify {
        summary: "Timer done".into(),
        body: "25:00 timer finished".into(),
    });
    assert!(contains(&body, b"Notify"), "variant name 'Notify' present");

    let effect = Effect::Notify {
        summary: "Timer done".into(),
        body: "Your 5:00 break is up".into(),
    };
    let back: Effect = decode(&encode(&effect)).expect("decode Notify");
    assert_eq!(effect, back);
}

#[test]
fn notify_capability_is_name_tagged() {
    // The paired manifest capability (#406) that gates the effect. Like every
    // other `Capability` it rides the wire as its bare variant name, so a
    // manifest requesting it round-trips and appending it is additive.
    let mut m = sample_manifest();
    m.capabilities = vec![Capability::Notify];
    let back: Manifest = decode(&encode(&m)).expect("decode manifest with Notify cap");
    assert_eq!(m, back);
    assert!(
        contains(&encode_body(&Capability::Notify), b"Notify"),
        "capability name 'Notify' rides the wire",
    );
}

// ── Consent (#487 phase 1b) ──────────────────────────────────────────────────

#[test]
fn request_consent_effect_is_name_tagged_and_additive() {
    // `RequestConsent` is a brand-new, externally-tagged `Effect` variant, so it
    // rides the wire as its bare variant *name* — the property that makes
    // appending it additive: an older decoder skips an unknown tag rather than
    // mis-decoding an existing variant, and every pre-1b frame decodes byte-for-
    // byte unchanged. So `PROTO_VERSION` stays put.
    assert_eq!(
        PROTO_VERSION, 1,
        "appending a variant must not bump the proto"
    );

    let effect = Effect::RequestConsent {
        request_id: 42,
        agent: "claude".into(),
        datasource: "departures".into(),
        scope: "*".into(),
        detail: "next S-Bahn departures".into(),
    };
    let body = encode_body(&effect);
    assert!(
        contains(&body, b"RequestConsent"),
        "variant name 'RequestConsent' present"
    );
    let back: Effect = decode(&encode(&effect)).expect("decode RequestConsent");
    assert_eq!(effect, back);
}

#[test]
fn consent_capability_is_name_tagged() {
    // The paired manifest capability (#487) that gates the effect *and* is the
    // #305 opt-in for the `ConsentDecision` push. Like every other `Capability`
    // it rides the wire as its bare variant name, so a manifest requesting it
    // round-trips and appending it is additive.
    let mut m = sample_manifest();
    m.capabilities = vec![Capability::Consent];
    let back: Manifest = decode(&encode(&m)).expect("decode manifest with Consent cap");
    assert_eq!(m, back);
    assert!(
        contains(&encode_body(&Capability::Consent), b"Consent"),
        "capability name 'Consent' rides the wire",
    );
}

#[test]
fn consent_decision_push_round_trips_every_variant() {
    // The paired host→plugin push (#487): a `HostMsg::ConsentDecision` carrying
    // each of the four decisions round-trips, and both the variant name and the
    // decision names ride the wire (name-tagged → appending is additive).
    assert!(
        contains(
            &encode_body(&HostMsg::ConsentDecision {
                request_id: 1,
                decision: ConsentDecision::Deny,
            }),
            b"ConsentDecision"
        ),
        "variant name 'ConsentDecision' rides the wire",
    );
    for (decision, name) in [
        (ConsentDecision::AllowOnce, b"AllowOnce".as_slice()),
        (ConsentDecision::AllowSession, b"AllowSession"),
        (ConsentDecision::AllowAlways, b"AllowAlways"),
        (ConsentDecision::Deny, b"Deny"),
    ] {
        let msg = HostMsg::ConsentDecision {
            request_id: 7,
            decision,
        };
        let back: HostMsg = decode(&encode(&msg)).expect("decode ConsentDecision");
        assert_eq!(msg, back, "{decision:?} round-trips");
        assert!(
            contains(&encode_body(&decision), name),
            "decision name rides the wire",
        );
    }
}

// ── Datasource protocol (#509) ───────────────────────────────────────────────

#[test]
fn datasource_effects_are_name_tagged_and_additive() {
    // The two new `Effect` variants ride the wire as their bare variant *names*,
    // so appending them is additive and `PROTO_VERSION` stays 1.
    assert_eq!(
        PROTO_VERSION, 1,
        "appending a variant must not bump the proto"
    );

    let query = Effect::DatasourceQuery {
        request_id: 1,
        provider: "departures".into(),
        scope: "next".into(),
        params: r#"{"limit":5}"#.into(),
    };
    assert!(
        contains(&encode_body(&query), b"DatasourceQuery"),
        "variant name 'DatasourceQuery' rides the wire",
    );
    assert_eq!(
        query,
        decode::<Effect>(&encode(&query)).expect("decode DatasourceQuery"),
    );

    let result = Effect::DatasourceResult {
        request_id: 1,
        outcome: DatasourceOutcome::Ready("payload".into()),
    };
    assert!(contains(&encode_body(&result), b"DatasourceResult"));
    assert_eq!(
        result,
        decode::<Effect>(&encode(&result)).expect("decode DatasourceResult"),
    );
}

#[test]
fn datasource_outcome_round_trips_every_shape() {
    // The success payload and each host-/provider-sourced error kind survive the
    // round-trip, and the error names ride the wire (name-tagged → additive).
    let cases = [
        DatasourceOutcome::Ready(r#"{"ok":true}"#.into()),
        DatasourceOutcome::Failed {
            error: DatasourceError::NotFound,
            message: "no provider".into(),
        },
        DatasourceOutcome::Failed {
            error: DatasourceError::ScopeDenied,
            message: "scope not served".into(),
        },
        DatasourceOutcome::Failed {
            error: DatasourceError::Timeout,
            message: "no answer".into(),
        },
        DatasourceOutcome::Failed {
            error: DatasourceError::Provider,
            message: "upstream failed".into(),
        },
    ];
    for outcome in cases {
        let msg = HostMsg::DatasourceResult {
            request_id: 7,
            outcome: outcome.clone(),
        };
        assert_eq!(
            msg,
            decode::<HostMsg>(&encode(&msg)).expect("decode DatasourceResult"),
            "{outcome:?} round-trips",
        );
    }
    for name in [
        b"NotFound".as_slice(),
        b"ScopeDenied",
        b"Timeout",
        b"Provider",
    ] {
        let outcome = DatasourceOutcome::Failed {
            error: match name {
                b"NotFound" => DatasourceError::NotFound,
                b"ScopeDenied" => DatasourceError::ScopeDenied,
                b"Timeout" => DatasourceError::Timeout,
                _ => DatasourceError::Provider,
            },
            message: String::new(),
        };
        assert!(
            contains(&encode_body(&outcome), name),
            "error name rides the wire",
        );
    }
}

#[test]
fn datasource_capabilities_and_provides_are_additive() {
    // The two new caps ride the wire as bare names, and the `provides` field is an
    // additive, skip-when-empty manifest field: a non-provider manifest is
    // byte-identical to a pre-#509 one, and a provider's round-trips.
    for cap in [Capability::DatasourceQuery, Capability::DatasourceProvider] {
        let mut m = sample_manifest();
        m.capabilities = vec![cap];
        assert_eq!(m, decode::<Manifest>(&encode(&m)).expect("decode cap"));
    }

    // A non-provider's `provides` is empty and skipped on the wire → byte-identical
    // to a pre-#509 manifest that never had the field.
    let plain = sample_manifest();
    assert!(plain.provides.is_empty());
    assert!(
        !contains(&encode_body(&plain), b"provides"),
        "an empty `provides` is skipped on the wire",
    );

    // A provider's manifest round-trips carrying its provided datasources + cap.
    let mut provider = sample_manifest();
    provider.capabilities = vec![Capability::DatasourceProvider];
    provider.provides = vec![
        ProvidedDatasource::new("departures", vec!["next".into()]),
        ProvidedDatasource::new("weather", vec!["current".into()]),
    ];
    let back: Manifest = decode(&encode(&provider)).expect("decode provider manifest");
    assert_eq!(provider, back);
    assert!(back.provides[0].serves_scope("next"));
    assert!(!back.provides[0].serves_scope("nope"));
}

// ── Plugin panel + PluginSelf page (#349 PR2) ────────────────────────────────

#[test]
fn page_pluginself_round_trips_and_is_name_tagged() {
    // `PluginSelf` is a brand-new, externally-tagged `Page` variant, so it rides
    // the wire as its bare variant *name* — the property that makes appending it
    // additive: an older decoder skips an unknown tag rather than mis-decoding an
    // existing variant, so `PROTO_VERSION` stays 1.
    assert_eq!(
        PROTO_VERSION, 1,
        "appending a variant must not bump the proto"
    );

    let effect = Effect::OpenPage(Page::PluginSelf);
    let back: Effect = decode(&encode(&effect)).expect("decode OpenPage(PluginSelf)");
    assert_eq!(effect, back);

    let body = encode_body(&effect);
    assert!(
        contains(&body, b"PluginSelf"),
        "variant name 'PluginSelf' rides the wire",
    );
}

#[test]
fn render_with_panel_round_trips() {
    // A `Render` carrying a distinct `panel` tree (a second, independent `Node`)
    // survives the round-trip alongside its chip tree and effects.
    let msg = PluginMsg::Render {
        tree: sample_tree(),
        panel: Some(Node::Label {
            id: Some("panel-lbl".into()),
            text: "panel body".into(),
            classes: vec![],
        }),
        effects: vec![Effect::OpenPage(Page::PluginSelf)],
    };
    let back: PluginMsg = decode(&encode(&msg)).expect("decode panel-bearing Render");
    assert_eq!(msg, back);
}

#[test]
fn render_without_panel_stays_off_the_wire() {
    // `panel: None` must NOT put a `panel` key on the wire
    // (`skip_serializing_if = "Option::is_none"`), so a panel-less frame is
    // byte-identical to a pre-PR2 frame and `PROTO_VERSION` stays 1.
    let msg = PluginMsg::Render {
        tree: sample_tree(),
        panel: None,
        effects: vec![],
    };
    let body = encode_body(&msg);
    assert!(
        !contains(&body, b"panel"),
        "an absent panel carries no panel key",
    );
}

#[test]
fn render_without_panel_decodes_old_frame_compat() {
    // A `Render` frame built before PR2 has no `panel` key. The current decoder
    // must still accept it, defaulting `panel` to `None` (`#[serde(default)]` +
    // named-map encoding) — the backward-compat guarantee that keeps an
    // already-deployed panel-less plugin rendering. Modeled as an
    // externally-tagged enum mirroring the pre-PR2 field set, so it serializes as
    // `{"Render": { tree, effects }}` exactly like an old plugin.
    #[derive(serde::Serialize)]
    enum PluginMsgOld {
        Render { tree: Node, effects: Vec<Effect> },
    }

    let old = PluginMsgOld::Render {
        tree: sample_tree(),
        effects: vec![Effect::OpenPage(Page::Media)],
    };
    let body = encode_body(&old);
    assert!(
        !contains(&body, b"panel"),
        "an old frame carries no panel key",
    );
    let decoded: PluginMsg = decode_body(&body).expect("decode pre-PR2 Render frame");
    assert_eq!(
        decoded,
        PluginMsg::Render {
            tree: sample_tree(),
            // Absent `panel` defaults to None (no plugin panel).
            panel: None,
            effects: vec![Effect::OpenPage(Page::Media)],
        },
        "absent panel defaults to None",
    );
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
    // Its map also predates `vocab` and `vocab_max`, so those default to
    // generation 0 and "does not negotiate" respectively.
    assert_eq!(
        decoded,
        Manifest {
            vocab: 0,
            vocab_max: None,
            ..sample_manifest()
        }
    );
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
    // The same old map also predates `vocab` and `vocab_max`, which default to
    // generation 0 and "does not negotiate".
    assert_eq!(
        decoded,
        Manifest {
            vocab: 0,
            vocab_max: None,
            ..sample_manifest()
        },
        "order defaults to None, vocab to generation 0, vocab_max to non-negotiating",
    );
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
fn manifest_without_vocab_decodes_to_generation_zero() {
    // #437: an older plugin (built before the `vocab` counter existed) sends a
    // Register whose manifest map has NO `vocab` key. The current host must still
    // decode it, defaulting `vocab` to `0` (relies on `#[serde(default)]` +
    // named-map encoding). Generation 0 is what those plugins always pass at.
    #[derive(serde::Serialize)]
    struct ManifestNoVocab {
        id: String,
        proto: u16,
        subscribes: Vec<StateKey>,
        capabilities: Vec<Capability>,
        mount: Mount,
    }

    let old = ManifestNoVocab {
        id: "vibectl".into(),
        proto: PROTO_VERSION,
        subscribes: vec![StateKey::Clock],
        capabilities: vec![Capability::OpenPage, Capability::RunCommand],
        mount: Mount::SidebarTop,
    };

    let body = encode_body(&old);
    assert!(
        !contains(&body, b"vocab"),
        "an old manifest carries no vocab key",
    );
    let decoded: Manifest = decode_body(&body).expect("decode a pre-vocab manifest");
    assert_eq!(decoded.vocab, 0, "absent vocab defaults to generation 0");
    // Generation 0 always clears the host's check (any host's VOCAB >= 0).
    decoded
        .check_vocab()
        .expect("a generation-0 (pre-vocab) plugin always passes check_vocab");
}

#[test]
fn manifest_vocab_is_stamped_and_rides_the_wire() {
    // `Manifest::new` stamps the vocabulary generations automatically, like
    // `proto` — a plugin author never sets them. Unlike `order`/`provides`,
    // `vocab` carries no `skip_serializing_if`, so it is always on the wire (a
    // host can always read the generation a plugin declares). A non-zero value
    // round-trips intact.
    //
    // Since #882 the stamped `vocab` is `VOCAB_UNCONDITIONAL`, **not** `VOCAB`:
    // it is the generation the plugin may emit with no host advertisement, and
    // it is what an older host exact-checks. The census `VOCAB` goes in
    // `vocab_max`, which is the negotiated ceiling — see `VOCAB_UNCONDITIONAL`'s
    // docs for why collapsing the two would make every rebuilt plugin fail an
    // older shell's `check_vocab` instead of degrading to `Node::Pixels`.
    let m = Manifest::new("caw", Mount::SidebarTop);
    assert_eq!(
        m.vocab, VOCAB_UNCONDITIONAL,
        "new() stamps the unconditional generation"
    );
    assert_eq!(
        m.vocab_max,
        Some(VOCAB),
        "new() stamps the negotiated ceiling"
    );
    assert!(
        contains(&encode_body(&m), b"vocab"),
        "vocab is always serialized (no skip_serializing_if)",
    );

    let mut newer = Manifest::new("future-plugin", Mount::SidebarTop);
    newer.vocab = 7;
    let back: Manifest = decode_body(&encode_body(&newer)).expect("decode a newer-vocab manifest");
    assert_eq!(back.vocab, 7, "a non-zero vocab round-trips");
}

#[test]
fn check_vocab_rejects_newer_and_accepts_same_or_older() {
    // #437: a plugin at the host's own vocabulary (or older, incl. the pre-vocab
    // generation 0) passes; one built against a *newer* vocabulary is refused with
    // a self-explanatory error naming both generations.
    let mut m = Manifest::new("p", Mount::SidebarTop);
    m.vocab = VOCAB;
    m.check_vocab().expect("same vocab is accepted");
    m.vocab = 0;
    m.check_vocab().expect("older (generation 0) is accepted");

    m.vocab = VOCAB + 1;
    let err = m.check_vocab().expect_err("a newer vocab is rejected");
    assert!(
        matches!(err, ProtoError::VocabTooNew { ours, theirs } if ours == VOCAB && theirs == VOCAB + 1),
    );
    let msg = err.to_string();
    assert!(
        msg.contains("newer") && msg.contains("update the shell"),
        "the error is self-explanatory: {msg}",
    );
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

// ── Float sanitisation (#904) ────────────────────────────────────────────────
//
// `Node` derives `PartialEq`, and both ends of the wire use that derive as
// their "did anything change?" test — so a `NaN` in a `Progress`/`Slider` float
// made a tree unequal to an identical copy of itself and spun both render-dedup
// loops forever. `Node::clamp_in_place` closes that; the mapping on its rustdoc
// is contract, and the tests below pin every row of it plus the four
// invariants: (a) finite and in range, (b) equal to itself, (c) a fixpoint,
// (d) valid input untouched.

/// A poison of the same class as `poison`, as an `f32` — for the preem child,
/// whose floats are `f32`. Spelled out rather than cast so the tests stay clear
/// of a lossy `f64 as f32`.
fn f32_poison(poison: f64) -> f32 {
    if poison.is_nan() {
        f32::NAN
    } else if poison.is_sign_positive() {
        f32::INFINITY
    } else {
        f32::NEG_INFINITY
    }
}

fn poisoned_progress(id: &str, poison: f64) -> Node {
    Node::Progress {
        id: Some(id.into()),
        fraction: poison,
        classes: vec![],
    }
}

fn poisoned_slider(id: &str, poison: f64) -> Node {
    Node::Slider {
        id: id.into(),
        min: poison,
        max: poison,
        value: poison,
        step: poison,
        enabled: true,
        classes: vec![],
    }
}

fn poisoned_led_strip(poison: f64) -> PreemWidget {
    let level = f32_poison(poison);
    PreemWidget::LedStrip {
        config: LedStripConfig::default(),
        state: LedStripState {
            level,
            peak: Some(level),
        },
    }
}

/// `inner` wrapped in every container the walker has to recurse through,
/// labelled — so a forgotten arm names itself instead of hiding behind a
/// sibling that happened to be visited.
fn containers_burying(inner: Node) -> Vec<(&'static str, Node)> {
    vec![
        ("bare", inner.clone()),
        (
            "box",
            Node::Box {
                id: None,
                dir: Dir::Vertical,
                spacing: 0,
                scroll: false,
                classes: vec![],
                children: vec![inner.clone()],
            },
        ),
        (
            "row",
            Node::Row {
                id: None,
                classes: vec![],
                children: vec![inner.clone()],
            },
        ),
        (
            "list-box",
            Node::ListBox {
                id: None,
                classes: vec![],
                children: vec![inner.clone()],
            },
        ),
        (
            "button",
            Node::Button {
                id: "btn".into(),
                classes: vec![],
                child: Box::new(inner.clone()),
            },
        ),
        (
            "revealer",
            Node::Revealer {
                id: None,
                open: true,
                child: Box::new(inner.clone()),
            },
        ),
        (
            "expander-header",
            Node::Expander {
                id: "exp".into(),
                header: Box::new(inner.clone()),
                children: vec![],
                expanded: true,
                classes: vec![],
            },
        ),
        (
            "expander-child",
            Node::Expander {
                id: "exp".into(),
                header: Box::new(Node::Spacer),
                children: vec![inner],
                expanded: true,
                classes: vec![],
            },
        ),
    ]
}

/// A tree carrying `poison` in **every** float it has — a `Progress` and a
/// `Slider` under each container, a poisoned preem child, and the float-less,
/// child-less variants for good measure. Every non-float field is legal, so a
/// failure below can only be about floats.
fn tree_with_every_float_poisoned(poison: f64) -> Node {
    let mut children: Vec<Node> = containers_burying(poisoned_progress("prog", poison))
        .into_iter()
        .chain(containers_burying(poisoned_slider("slide", poison)))
        .map(|(_, node)| node)
        .collect();
    children.push(Node::Preem {
        id: Some("led".into()),
        classes: vec![],
        widget: Box::new(poisoned_led_strip(poison)),
    });
    // The float-less, child-less variants, so the walker's inert arms are
    // exercised by every test below rather than only by the ones looking for
    // them.
    children.extend([
        Node::Label {
            id: None,
            text: "hi".into(),
            classes: vec![],
        },
        Node::Text {
            id: None,
            text: "hi".into(),
            max_width_chars: None,
            ellipsize: false,
            classes: vec![],
        },
        Node::Icon {
            id: None,
            name: "weather-clear-symbolic".into(),
            classes: vec![],
        },
        Node::Pixels {
            id: None,
            width: 1,
            height: 1,
            data: vec![0, 0, 0, 0],
            scale: 1,
            classes: vec![],
        },
        Node::Separator { classes: vec![] },
        Node::Spacer,
        Node::Entry {
            id: "entry".into(),
            text: String::new(),
            placeholder: String::new(),
            classes: vec![],
        },
    ]);
    Node::Box {
        id: Some("root".into()),
        dir: Dir::Vertical,
        spacing: 6,
        scroll: false,
        classes: vec![],
        children,
    }
}

/// Exact float equality by bit pattern — stricter than `==` (which would let a
/// `-0.0` → `0.0` rewrite pass) and lint-clean, unlike a bare float compare.
#[track_caller]
fn assert_f64_bits(got: f64, want: f64, what: &str) {
    assert_eq!(
        got.to_bits(),
        want.to_bits(),
        "{what}: got {got}, want {want}"
    );
}

#[track_caller]
fn assert_finite_in(value: f64, lo: f64, hi: f64, what: &str) {
    assert!(value.is_finite(), "{what} is not finite: {value}");
    assert!(
        value >= lo && value <= hi,
        "{what} = {value}, want {lo}..={hi}"
    );
}

/// Invariant (a) for the `f64`s this vocabulary owns: recurse the tree and
/// check every `Progress`/`Slider` float against its documented bounds. A
/// `Preem` child's `f32`s have their own invariant, their own mapping and their
/// own tests (`tests/preem.rs`); the delegation to them is pinned separately
/// below.
#[track_caller]
fn assert_node_floats_are_sane(node: &Node) {
    match node {
        Node::Progress { fraction, .. } => assert_finite_in(*fraction, 0.0, 1.0, "fraction"),
        Node::Slider {
            min,
            max,
            value,
            step,
            ..
        } => {
            assert!(min.is_finite() && max.is_finite(), "range {min}..={max}");
            assert!(max > min, "degenerate range {min}..={max}");
            assert!(
                (max - min).is_finite(),
                "range span overflows: {min}..={max}"
            );
            assert_finite_in(*value, *min, *max, "value");
            assert!(
                step.is_finite() && *step > 0.0,
                "step is not finite and positive: {step}"
            );
            assert!(
                *step <= max - min,
                "step {step} is wider than the span {}",
                max - min
            );
        }
        Node::Box { children, .. }
        | Node::Row { children, .. }
        | Node::ListBox { children, .. } => {
            for child in children {
                assert_node_floats_are_sane(child);
            }
        }
        Node::Button { child, .. } | Node::Revealer { child, .. } => {
            assert_node_floats_are_sane(child);
        }
        Node::Expander {
            header, children, ..
        } => {
            assert_node_floats_are_sane(header);
            for child in children {
                assert_node_floats_are_sane(child);
            }
        }
        Node::Label { .. }
        | Node::Text { .. }
        | Node::Icon { .. }
        | Node::Pixels { .. }
        | Node::Separator { .. }
        | Node::Spacer
        | Node::Entry { .. }
        | Node::Preem { .. } => {}
    }
}

/// Every `f64` in a tree, as bit patterns, in walk order — for the identity
/// check below, which wants *exact* comparison rather than `==`.
fn node_float_bits(node: &Node) -> Vec<u64> {
    match node {
        Node::Progress { fraction, .. } => vec![fraction.to_bits()],
        Node::Slider {
            min,
            max,
            value,
            step,
            ..
        } => vec![
            min.to_bits(),
            max.to_bits(),
            value.to_bits(),
            step.to_bits(),
        ],
        Node::Box { children, .. }
        | Node::Row { children, .. }
        | Node::ListBox { children, .. } => children.iter().flat_map(node_float_bits).collect(),
        Node::Button { child, .. } | Node::Revealer { child, .. } => node_float_bits(child),
        Node::Expander {
            header, children, ..
        } => node_float_bits(header)
            .into_iter()
            .chain(children.iter().flat_map(node_float_bits))
            .collect(),
        Node::Label { .. }
        | Node::Text { .. }
        | Node::Icon { .. }
        | Node::Pixels { .. }
        | Node::Separator { .. }
        | Node::Spacer
        | Node::Entry { .. }
        | Node::Preem { .. } => Vec::new(),
    }
}

/// The probe is real: an **unclamped** tree carrying a `NaN` is not equal to
/// itself, which is exactly the defect the clamp exists to close. Without this,
/// a clamp that silently did nothing could still pass the equality tests below
/// on a tree that never carried a `NaN`.
#[test]
fn an_unclamped_nan_node_is_not_equal_to_itself() {
    let tree = tree_with_every_float_poisoned(f64::NAN);
    assert_ne!(
        tree.clone(),
        tree,
        "the poison probe reached no float at all"
    );
    let progress = poisoned_progress("prog", f64::NAN);
    assert_ne!(
        progress.clone(),
        progress,
        "Progress carries no NaN — the probe missed its field"
    );
    let slider = poisoned_slider("slide", f64::NAN);
    assert_ne!(
        slider.clone(),
        slider,
        "Slider carries no NaN — the probe missed its fields"
    );
}

/// Invariant (a): after clamping, every float of every node is finite and
/// inside its bounds — even when the input carried a `NaN` or an infinity in
/// *every* float field it has.
#[test]
fn poisoned_node_floats_are_finite_and_in_range_after_clamping() {
    for poison in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_node_floats_are_sane(&tree_with_every_float_poisoned(poison).clamped());
    }
}

/// The walk is total: a poisoned node buried in **any** container comes back
/// sanitised, so a forgotten recursion arm fails here (naming the container)
/// rather than passing because a sibling happened to be visited.
#[test]
fn the_clamp_reaches_a_node_buried_in_every_container() {
    for poison in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        for inner in [
            poisoned_progress("prog", poison),
            poisoned_slider("slide", poison),
        ] {
            for (label, node) in containers_burying(inner.clone()) {
                let clamped = node.clone().clamped();
                assert_eq!(
                    clamped.clone(),
                    node.clamped(),
                    "a poisoned node under a {label} is not equal to itself after clamping"
                );
                assert_node_floats_are_sane(&clamped);
            }
        }
    }
}

/// Invariant (b): a clamped tree compares equal to itself, however poisoned the
/// input was. This is the property both ends' render dedup rests on — the SDK's
/// `view != last_view` and the host reconciler's node diff.
#[test]
fn a_clamped_node_compares_equal_to_itself() {
    for poison in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let tree = tree_with_every_float_poisoned(poison);
        assert_eq!(
            tree.clone().clamped(),
            tree.clamped(),
            "a clamped tree does not compare equal to itself"
        );
    }
}

/// Invariant (c): clamping is a **fixpoint**, so a host that clamps a tree the
/// SDK already clamped must not see it move — or the equality gates would fire
/// on the second pass instead of the first.
#[test]
fn clamping_a_node_is_a_fixpoint_even_when_poisoned() {
    for poison in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let once = tree_with_every_float_poisoned(poison).clamped();
        assert_eq!(
            once.clone().clamped(),
            once,
            "clamping twice is not the same as clamping once"
        );
    }
}

/// Invariant (d): the sanitiser must not perturb valid input. Bit-exact, which
/// is stricter than `==` — the latter would let a `-0.0` → `0.0` rewrite
/// through.
#[test]
fn clamping_a_legal_node_is_bit_identical() {
    let tree = sample_tree();
    let clamped = tree.clone().clamped();
    assert_eq!(
        node_float_bits(&clamped),
        node_float_bits(&tree),
        "a legal tree's floats moved"
    );
    assert_eq!(clamped, tree, "a legal tree was mutated by clamped()");
    // …and a legal preem child, whose floats this walker delegates rather than
    // owns (`tests/preem.rs` pins the bit-identity of those).
    let preem = Node::Preem {
        id: Some("led".into()),
        classes: vec![],
        widget: Box::new(PreemWidget::LedStrip {
            config: LedStripConfig::default(),
            state: LedStripState {
                level: 0.5,
                peak: Some(0.75),
            },
        }),
    };
    assert_eq!(
        preem.clone().clamped(),
        preem,
        "a legal preem child was mutated"
    );
}

/// The `Progress::fraction` row of the mapping table: `±inf` saturates exactly
/// as `GtkProgressBar`'s own `CLAMP` does (`gtkprogressbar.c:781`), a finite
/// out-of-range fraction clamps the same way, and `NaN` — the one input that
/// `CLAMP` passes straight through, leaving GTK to multiply it into an `int`
/// allocation — takes the empty bar.
#[test]
fn non_finite_progress_fractions_map_to_their_documented_replacements() {
    assert_f64_bits(sane_fraction(f64::NAN), 0.0, "NaN fraction");
    assert_f64_bits(sane_fraction(f64::INFINITY), 1.0, "+inf fraction");
    assert_f64_bits(sane_fraction(f64::NEG_INFINITY), 0.0, "-inf fraction");
    assert_f64_bits(sane_fraction(2.5), 1.0, "over-range fraction");
    assert_f64_bits(sane_fraction(-2.5), 0.0, "under-range fraction");
    assert_f64_bits(sane_fraction(0.42), 0.42, "a legal fraction");
    // …and through the node seam, not only the free function.
    let Node::Progress { fraction, .. } = poisoned_progress("prog", f64::NAN).clamped() else {
        panic!("clamping changed the variant")
    };
    assert_f64_bits(fraction, 0.0, "NaN fraction through Node::clamped");
}

/// The `Slider::value` and `Slider::step` rows, against a legal **non-default**
/// scale so a row that quietly fell back to the unit range would fail here
/// rather than pass by coincidence.
#[test]
fn non_finite_slider_floats_map_to_their_documented_replacements() {
    let (min, max) = (-20.0_f64, 40.0_f64);
    let span = max - min;
    for (poison, want) in [
        (f64::NAN, min),
        (f64::INFINITY, max),
        (f64::NEG_INFINITY, min),
    ] {
        let sane = sane_slider_floats(min, max, poison, 1.0);
        assert_f64_bits(sane.min, min, "min beside a poisoned value");
        assert_f64_bits(sane.max, max, "max beside a poisoned value");
        assert_f64_bits(sane.value, want, "value");
        assert_f64_bits(sane.step, 1.0, "step beside a poisoned value");
    }
    // A finite out-of-range value clamps to the end — parity with
    // `gtk_adjustment_sanitize_value` (`gtkadjustment.c:365`-`:372`).
    assert_f64_bits(
        sane_slider_floats(min, max, 100.0, 1.0).value,
        max,
        "over-range value",
    );
    assert_f64_bits(
        sane_slider_floats(min, max, -100.0, 1.0).value,
        min,
        "under-range value",
    );
    // Every unusable step takes the documented fraction of the span — the
    // non-finite ones GTK refuses, and the non-positive ones it accepts but
    // which leave the arrow keys dead (`0.0`) or inverted (`< 0.0`).
    let want_step = span * DEFAULT_SLIDER_STEP_FRACTION;
    for bad in [
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        0.0,
        -0.0,
        -1.0,
        -1e300,
    ] {
        assert_f64_bits(
            sane_slider_floats(min, max, 0.0, bad).step,
            want_step,
            "fallback step",
        );
    }
    // …a legal one is untouched, and one wider than the span caps to it.
    assert_f64_bits(
        sane_slider_floats(min, max, 0.0, 2.5).step,
        2.5,
        "a legal step",
    );
    assert_f64_bits(
        sane_slider_floats(min, max, 0.0, 1e9).step,
        span,
        "an over-wide step",
    );
    // …and through the node seam, not only the free function.
    let Node::Slider {
        min: got_min,
        max: got_max,
        value,
        step,
        ..
    } = poisoned_slider("slide", f64::NAN).clamped()
    else {
        panic!("clamping changed the variant")
    };
    assert_f64_bits(got_min, DEFAULT_SLIDER_MIN, "min through Node::clamped");
    assert_f64_bits(got_max, DEFAULT_SLIDER_MAX, "max through Node::clamped");
    assert_f64_bits(value, DEFAULT_SLIDER_MIN, "value through Node::clamped");
    assert_f64_bits(
        step,
        (DEFAULT_SLIDER_MAX - DEFAULT_SLIDER_MIN) * DEFAULT_SLIDER_STEP_FRACTION,
        "step through Node::clamped",
    );
}

/// The `Slider::min`/`max` row: every degenerate shape falls back to the unit
/// scale, **both ends as a unit**, and an inverted range is never silently
/// swapped.
#[test]
fn a_degenerate_slider_range_falls_back_to_the_unit_scale() {
    let degenerate = [
        ("NaN min", f64::NAN, 1.0),
        ("NaN max", 0.0, f64::NAN),
        ("+inf max", 0.0, f64::INFINITY),
        ("-inf min", f64::NEG_INFINITY, 1.0),
        ("both infinite", f64::NEG_INFINITY, f64::INFINITY),
        ("inverted", 10.0, 5.0),
        ("empty", 5.0, 5.0),
        ("overflowing span", -f64::MAX, f64::MAX),
    ];
    for (what, min, max) in degenerate {
        let sane = sane_slider_floats(min, max, 0.7, 0.05);
        assert_f64_bits(sane.min, DEFAULT_SLIDER_MIN, &format!("{what}: min"));
        assert_f64_bits(sane.max, DEFAULT_SLIDER_MAX, &format!("{what}: max"));
        // The range is replaced as a unit; the value and the step survive here
        // only because both are legal against the fallback scale.
        assert_f64_bits(sane.value, 0.7, &format!("{what}: value"));
        assert_f64_bits(sane.step, 0.05, &format!("{what}: step"));
    }
    // Never swapped: `10.0..=5.0` does not become `5.0..=10.0`, so a plugin's
    // transposed arguments cannot turn into a working-but-backwards slider.
    let inverted = sane_slider_floats(10.0, 5.0, 7.0, 1.0);
    assert_f64_bits(inverted.min, DEFAULT_SLIDER_MIN, "an inverted range's min");
    assert_f64_bits(inverted.max, DEFAULT_SLIDER_MAX, "an inverted range's max");
    assert_f64_bits(
        inverted.value,
        DEFAULT_SLIDER_MAX,
        "a value above the fallback scale clamps to its top",
    );
    // A legal range — including a negative one — is left exactly alone.
    assert_eq!(
        sane_slider_floats(-20.0, 40.0, 0.0, 1.0),
        SliderFloats {
            min: -20.0,
            max: 40.0,
            value: 0.0,
            step: 1.0,
        },
        "a legal range moved"
    );
}

/// The `step` bound holds for every combination, including the subnormal spans
/// where one percent of the span underflows to zero.
#[test]
fn a_sanitised_step_is_always_positive_finite_and_within_the_span() {
    let ranges = [
        (0.0, 1.0),
        (-20.0, 40.0),
        (f64::NAN, 1.0),
        (10.0, 5.0),
        (0.0, f64::MIN_POSITIVE),
        // The smallest subnormal span: `span * 0.01` rounds to zero here.
        (0.0, f64::from_bits(1)),
        (-f64::MAX, f64::MAX),
    ];
    let steps = [
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        0.0,
        -1.0,
        1e300,
        f64::MIN_POSITIVE,
        0.5,
    ];
    for (min, max) in ranges {
        for step in steps {
            let sane = sane_slider_floats(min, max, 0.0, step);
            assert!(
                sane.step.is_finite() && sane.step > 0.0,
                "{min}..={max} step {step} gave a non-positive/non-finite {}",
                sane.step
            );
            assert!(
                sane.step <= sane.max - sane.min,
                "{min}..={max} step {step} gave {} beyond the span {}",
                sane.step,
                sane.max - sane.min
            );
        }
    }
}

/// The `Preem` arm delegates rather than reimplements: the node clamp's answer
/// for a preem child is **exactly** `PreemWidget::clamped`'s, so the two
/// mappings cannot drift.
#[test]
fn the_node_clamp_delegates_a_preem_child_to_the_widget_clamp() {
    for poison in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let widget = poisoned_led_strip(poison);
        let node = Node::Preem {
            id: Some("led".into()),
            classes: vec![],
            widget: Box::new(widget.clone()),
        };
        let Node::Preem {
            widget: clamped, ..
        } = node.clamped()
        else {
            panic!("clamping changed the variant")
        };
        assert_eq!(
            *clamped,
            widget.clamped(),
            "the node clamp is not the widget clamp"
        );
    }
}

/// Wire compatibility: sanitisation happens **after** decode, never inside
/// serde. `NaN` and `±inf` are legal `MessagePack` floats, so a poisoned frame
/// must still decode — and decode must hand the poison through untouched, or
/// the decoder would be silently rewriting the wire.
#[test]
fn a_poisoned_node_round_trips_unsanitised_and_the_clamp_fixes_it_after_decode() {
    let tree = Node::Box {
        id: None,
        dir: Dir::Horizontal,
        spacing: 0,
        scroll: false,
        classes: vec![],
        children: vec![
            poisoned_progress("prog", f64::NAN),
            poisoned_slider("slide", f64::INFINITY),
        ],
    };
    let frame = PluginMsg::Render {
        tree,
        panel: None,
        effects: vec![],
    };
    let back: PluginMsg = decode(&encode(&frame)).expect("a poisoned frame still decodes");
    let PluginMsg::Render { tree: decoded, .. } = back else {
        panic!("not a Render")
    };
    let Node::Box { children, .. } = &decoded else {
        panic!("not a Box")
    };
    let Node::Progress { fraction, .. } = &children[0] else {
        panic!("not a Progress")
    };
    assert!(
        fraction.is_nan(),
        "decode must not sanitise — that is the clamp's job, after decode"
    );
    let Node::Slider { min, .. } = &children[1] else {
        panic!("not a Slider")
    };
    assert!(
        min.is_infinite(),
        "decode must not sanitise — that is the clamp's job, after decode"
    );
    // …and the clamp, applied after decode, is what makes it harmless.
    assert_node_floats_are_sane(&decoded.clamped());
}
