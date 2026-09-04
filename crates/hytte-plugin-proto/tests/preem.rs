//! The preem widget state vocabulary (#882): round-trips, wire limits,
//! negotiation, and the encoding-stability pins.
//!
//! Three things are under test here, in rising order of how badly a regression
//! would hurt:
//!
//! 1. **Round-trip** — every [`PreemWidget`] variant encodes and decodes back to
//!    an equal value, and every config/state struct's `#[serde(default)]`
//!    container attribute makes a partial map decode to the kit's defaults.
//! 2. **Wire limits** — [`PreemWidget::clamped`] actually enforces every
//!    documented cap, including on a char boundary for text.
//! 3. **Encoding stability** — adding [`Node::Preem`] must not have moved any
//!    *existing* node's bytes. [`existing_node_encodings_are_frozen`] pins two
//!    of them against hex literals recorded before the variant existed, so an
//!    accidental switch away from external (name-keyed) enum tagging fails loud
//!    here rather than silently bricking every deployed plugin.

use hytte_plugin_proto::{
    AccentRole, Cls, DotMatrixConfig, DotMatrixState, FlipBoardConfig, FlipBoardState, GaugeConfig,
    GaugeRange, GaugeState, HostMsg, LedStripConfig, LedStripState, MAX_BUFFER_DIM, MAX_CELLS,
    MAX_LEDS, MAX_SCALE, MAX_SCOPE_SAMPLES, MAX_TEXT_LEN, Manifest, MarqueeConfig, MarqueeState,
    Mechanism, Mount, Node, PREEM_VOCAB, PeakHoldConfig, PluginMsg, PreemWidget, ScopeConfig,
    ScopeState, SevenSegConfig, SevenSegState, StyleName, StyleRef, TextBoxConfig, TextBoxState,
    TextBoxWidth, VOCAB, VOCAB_UNCONDITIONAL, decode, decode_body, encode, encode_body, preem,
    preem_id, preem_styled,
};

// ── helpers ─────────────────────────────────────────────────────────────────

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

fn from_hex(s: &str) -> Vec<u8> {
    let s = s.trim();
    assert!(s.len().is_multiple_of(2), "hex needs an even digit count");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex digits only"))
        .collect()
}

/// One populated instance of **every** [`PreemWidget`] variant, each with
/// deliberately non-default config and state so a dropped or defaulted field
/// shows up as an inequality rather than passing by coincidence.
fn all_widgets() -> Vec<PreemWidget> {
    vec![
        PreemWidget::DotMatrix {
            config: DotMatrixConfig {
                style: StyleRef::new(StyleName::Vfd),
            },
            state: DotMatrixState {
                text: "12:34".into(),
            },
        },
        PreemWidget::SevenSeg {
            config: SevenSegConfig {
                style: StyleRef::new(StyleName::Lcd).with_accent(AccentRole::Warning),
            },
            state: SevenSegState {
                text: "88.8".into(),
            },
        },
        PreemWidget::TextBox {
            config: TextBoxConfig {
                style: StyleRef::new(StyleName::Oled).with_accent(AccentRole::Accent),
                width: TextBoxWidth::FitPx(268),
                max_lines: 4,
                pad: 4,
                corner: 3,
                scale: 2,
                fixed_width: true,
            },
            state: TextBoxState {
                text: "mrrp! the cat has opinions".into(),
            },
        },
        PreemWidget::LedStrip {
            config: LedStripConfig {
                style: StyleRef::new(StyleName::Crt),
                leds: 32,
                peak_hold: Some(PeakHoldConfig { rate: 0.02 }),
            },
            state: LedStripState {
                level: 0.62,
                peak: Some(0.9),
            },
        },
        PreemWidget::Marquee {
            config: MarqueeConfig {
                style: StyleRef::new(StyleName::Vfd).with_accent(AccentRole::Neutral),
                window_px: 268,
                gap_dots: 8,
                speed_dots_per_sec: 24.5,
            },
            state: MarqueeState {
                text: "Boards of Canada — Roygbiv".into(),
            },
        },
        PreemWidget::Scope {
            config: ScopeConfig {
                style: StyleRef::new(StyleName::Crt),
                cols: 128,
                rows: 40,
                scale: 3,
                persistence: 200,
            },
            state: ScopeState {
                samples: vec![0.0, 0.5, -0.5, 1.0, -1.0],
            },
        },
        PreemWidget::Gauge {
            config: GaugeConfig {
                style: StyleRef::new(StyleName::Lcd).with_accent(AccentRole::Error),
                cols: 160,
                rows: 72,
                scale: 1,
                sweep_deg: 120.0,
                divisions: 6,
                subdivisions: 4,
                range: GaugeRange {
                    low: -20.0,
                    high: 40.0,
                },
                frequency_hz: 3.5,
                damping: 0.7,
            },
            state: GaugeState { target: 21.5 },
        },
        PreemWidget::FlipBoard {
            config: FlipBoardConfig {
                style: StyleRef::new(StyleName::Oled),
                mechanism: Mechanism::Nixie,
                cells: 12,
                glyph_px: 4,
                scale: 1,
                duration_secs: Some(0.25),
                stagger_secs: Some(0.0),
            },
            state: FlipBoardState {
                text: "SPANDAU 12".into(),
            },
        },
    ]
}

// ── 1. round-trip ───────────────────────────────────────────────────────────

/// Every [`PreemWidget`] variant survives an encode→decode round-trip intact,
/// both bare and wrapped in the [`Node::Preem`] node it rides in.
#[test]
fn every_preem_widget_round_trips() {
    let widgets = all_widgets();
    assert_eq!(
        widgets.len(),
        8,
        "the draft's table has eight widgets — a ninth needs a case here"
    );

    for widget in widgets {
        let back = decode::<PreemWidget>(&encode(&widget)).expect("widget decodes");
        assert_eq!(back, widget, "{} did not round-trip", widget.kind());

        let node = preem_styled(
            format!("w-{}", widget.kind()),
            vec![Cls::from("ts-preem")],
            widget.clone(),
        );
        let back = decode::<Node>(&encode(&node)).expect("node decodes");
        assert_eq!(back, node, "Node::Preem({}) did not round-trip", widget.kind());
    }
}

/// A whole [`PluginMsg::Render`] carrying preem nodes among ordinary ones
/// round-trips — the shape a real plugin actually sends.
#[test]
fn render_frame_mixing_preem_and_legacy_nodes_round_trips() {
    let msg = PluginMsg::Render {
        tree: Node::Box {
            id: Some("root".into()),
            dir: hytte_plugin_proto::Dir::Vertical,
            spacing: 6,
            scroll: false,
            classes: vec!["ts-card".into()],
            children: vec![
                Node::Label {
                    id: None,
                    text: "now playing".into(),
                    classes: vec![],
                },
                preem_id("marquee", all_widgets()[4].clone()),
                preem(all_widgets()[3].clone()),
            ],
        },
        panel: None,
        effects: vec![],
    };
    let back = decode::<PluginMsg>(&encode(&msg)).expect("render frame decodes");
    assert_eq!(back, msg);
}

/// The node constructors differ only in identity and classes — `preem` is
/// anonymous, `preem_id` keys the renderer instance, `preem_styled` adds classes.
#[test]
fn preem_constructors_set_identity_and_classes() {
    let widget = all_widgets()[0].clone();
    assert_eq!(
        preem(widget.clone()),
        Node::Preem {
            id: None,
            classes: vec![],
            widget: widget.clone(),
        }
    );
    assert_eq!(
        preem_id("clock", widget.clone()),
        Node::Preem {
            id: Some("clock".into()),
            classes: vec![],
            widget: widget.clone(),
        }
    );
    assert_eq!(
        preem_styled("clock", vec!["a".into()], widget.clone()),
        Node::Preem {
            id: Some("clock".into()),
            classes: vec!["a".into()],
            widget,
        }
    );
}

/// The wire shape is the documented one: a name-keyed variant map whose body
/// carries exactly the two keys `config` and `state`, so the config-vs-state
/// split is visible to a non-Rust decoder reading the schema.
#[test]
fn preem_widget_encodes_as_named_config_and_state() {
    for widget in all_widgets() {
        let bytes = encode(&widget);
        let hay = String::from_utf8_lossy(&bytes).into_owned();
        for needle in ["config", "state"] {
            assert!(
                hay.contains(needle),
                "{}'s encoding is missing the `{needle}` key — the config/state \
                 split must stay visible on the wire",
                widget.kind()
            );
        }
    }
}

/// Every config and state struct carries a container-level `#[serde(default)]`,
/// so a peer that omits a key — an older SDK, or a non-Rust encoder that only
/// sets what it cares about — decodes to the kit's own default rather than
/// failing. This is what keeps *adding* a config field additive.
#[test]
fn omitted_config_keys_decode_to_kit_defaults() {
    // A `TextBox` whose config is an empty map and whose state carries only
    // `text` — every other key absent.
    let sparse = rmp_serde::to_vec_named(&SparseTextBox {
        text_box: SparseBody {
            config: Empty {},
            state: OnlyText { text: "hi".into() },
        },
    })
    .expect("sparse fixture encodes");

    let back = decode_body::<PreemWidget>(&sparse).expect("a sparse preem widget still decodes");
    assert_eq!(
        back,
        PreemWidget::TextBox {
            config: TextBoxConfig::default(),
            state: TextBoxState { text: "hi".into() },
        },
        "omitted keys must fall back to the kit's documented defaults"
    );
}

/// The defaults this vocabulary documents are the kit's own
/// (`crates/hytte-preem/src/*`), pinned so a drift in either direction is loud.
#[test]
fn documented_defaults_match_the_kit() {
    assert_eq!(
        TextBoxConfig::default(),
        TextBoxConfig {
            style: StyleRef::default(),
            width: TextBoxWidth::Cols(16),
            max_lines: 3,
            pad: 3,
            corner: 2,
            scale: 1,
            fixed_width: false,
        },
        "textbox.rs:55-67"
    );
    assert_eq!(
        LedStripConfig::default(),
        LedStripConfig {
            style: StyleRef::default(),
            leds: 24,
            peak_hold: None,
        },
        "led_strip.rs:36 DEFAULT_LEDS"
    );
    assert_eq!(
        MarqueeConfig::default(),
        MarqueeConfig {
            style: StyleRef::default(),
            window_px: 192,
            gap_dots: 6,
            speed_dots_per_sec: 20.0,
        },
        "marquee.rs:80-84 + the audio widget's ≈20 dots/s (main.rs:134-144)"
    );
    assert_eq!(
        ScopeConfig::default(),
        ScopeConfig {
            style: StyleRef::default(),
            cols: 144,
            rows: 48,
            scale: 2,
            persistence: 184,
        },
        "scope.rs:60-72"
    );
    assert_eq!(
        GaugeConfig::default(),
        GaugeConfig {
            style: StyleRef::default(),
            cols: 144,
            rows: 64,
            scale: 2,
            sweep_deg: 150.0,
            divisions: 4,
            subdivisions: 5,
            range: GaugeRange {
                low: 0.0,
                high: 1.0
            },
            frequency_hz: 2.0,
            damping: 0.5,
        },
        "gauge.rs:97-104, 375-395"
    );
    assert_eq!(
        FlipBoardConfig::default(),
        FlipBoardConfig {
            style: StyleRef::default(),
            mechanism: Mechanism::SplitFlap,
            cells: 8,
            glyph_px: 2,
            scale: 2,
            duration_secs: None,
            stagger_secs: None,
        },
        "split_flap.rs:135-144 (duration/stagger are None = the mechanism's own)"
    );
    assert_eq!(StyleRef::default(), StyleRef::new(StyleName::Vfd));
}

/// The name mappings mirror the kit's `DisplayStyle::name` / `Mechanism::name`,
/// so a host can map either way by name instead of by ordering.
#[test]
fn style_and_mechanism_names_mirror_the_kit() {
    assert_eq!(
        StyleName::ALL.map(StyleName::name),
        ["vfd", "lcd", "oled", "crt"]
    );
    assert_eq!(Mechanism::ALL.map(Mechanism::name), ["split-flap", "nixie"]);
}

/// [`PreemWidget::style`] reaches the shared field on every variant — the
/// accessor the shell's re-tint path leans on.
#[test]
fn style_accessor_covers_every_variant() {
    for widget in all_widgets() {
        let style = widget.style();
        assert!(
            StyleName::ALL.contains(&style.style),
            "{} returned a style outside the vocabulary",
            widget.kind()
        );
    }
}

// ── 2. wire limits ──────────────────────────────────────────────────────────

/// The text cap is enforced, and enforced on a **char boundary**: a naive
/// `String::truncate` at [`MAX_TEXT_LEN`] would panic mid-codepoint, which is
/// precisely the "malformed input must never crash the shell" failure this cap
/// exists to prevent.
#[test]
fn text_cap_truncates_on_a_char_boundary() {
    // 3-byte chars, so MAX_TEXT_LEN (4096) lands *inside* a codepoint:
    // 4096 = 3 * 1365 + 1.
    let long = "☃".repeat(2000);
    assert!(long.len() > MAX_TEXT_LEN);

    let clamped = PreemWidget::DotMatrix {
        config: DotMatrixConfig::default(),
        state: DotMatrixState { text: long },
    }
    .clamped();

    let PreemWidget::DotMatrix { state, .. } = clamped else {
        panic!("clamped() must not change the variant");
    };
    assert!(state.text.len() <= MAX_TEXT_LEN, "text cap not enforced");
    assert_eq!(state.text.len(), 3 * 1365, "must cut at the char boundary");
    assert!(state.text.chars().all(|c| c == '☃'), "no split codepoint");
}

/// Every text-carrying widget honours [`MAX_TEXT_LEN`] — not just the first one
/// somebody wired up. (A sibling that silently skips the cap is exactly the
/// partial-fix shape a per-widget sweep catches and a single-case test does not.)
#[test]
fn every_text_carrying_widget_enforces_the_text_cap() {
    let long = "x".repeat(MAX_TEXT_LEN * 2);
    let cases: Vec<PreemWidget> = vec![
        PreemWidget::DotMatrix {
            config: DotMatrixConfig::default(),
            state: DotMatrixState { text: long.clone() },
        },
        PreemWidget::SevenSeg {
            config: SevenSegConfig::default(),
            state: SevenSegState { text: long.clone() },
        },
        PreemWidget::TextBox {
            config: TextBoxConfig::default(),
            state: TextBoxState { text: long.clone() },
        },
        PreemWidget::Marquee {
            config: MarqueeConfig::default(),
            state: MarqueeState { text: long.clone() },
        },
        PreemWidget::FlipBoard {
            config: FlipBoardConfig::default(),
            state: FlipBoardState { text: long },
        },
    ];

    for case in cases {
        let kind = case.kind();
        let text_len = match case.clamped() {
            PreemWidget::DotMatrix { state, .. } => state.text.len(),
            PreemWidget::SevenSeg { state, .. } => state.text.len(),
            PreemWidget::TextBox { state, .. } => state.text.len(),
            PreemWidget::Marquee { state, .. } => state.text.len(),
            PreemWidget::FlipBoard { state, .. } => state.text.len(),
            other => panic!("clamped() changed the variant to {}", other.kind()),
        };
        assert!(
            text_len <= MAX_TEXT_LEN,
            "{kind} does not enforce MAX_TEXT_LEN"
        );
    }
}

/// A sample batch past [`MAX_SCOPE_SAMPLES`] is truncated, not rejected — the
/// `Node::Pixels` posture: a bad frame renders something sane, never drops the
/// session.
#[test]
fn scope_samples_are_capped_per_update() {
    let widget = PreemWidget::Scope {
        config: ScopeConfig::default(),
        state: ScopeState {
            samples: vec![0.25; MAX_SCOPE_SAMPLES * 3],
        },
    }
    .clamped();

    let PreemWidget::Scope { state, .. } = widget else {
        panic!("clamped() must not change the variant");
    };
    assert_eq!(state.samples.len(), MAX_SCOPE_SAMPLES);
}

/// The allocation-sizing knobs clamp into their documented ranges from **both**
/// directions: an absurd count is capped, and a zero is raised to the minimum
/// the kit needs (a 0×0 buffer or a 0× scale is not a widget).
#[test]
fn geometry_knobs_clamp_from_both_directions() {
    let huge = PreemWidget::Scope {
        config: ScopeConfig {
            cols: u32::MAX,
            rows: u32::MAX,
            scale: u32::MAX,
            ..ScopeConfig::default()
        },
        state: ScopeState::default(),
    }
    .clamped();
    let PreemWidget::Scope { config, .. } = huge else {
        panic!("variant changed")
    };
    assert_eq!((config.cols, config.rows), (MAX_BUFFER_DIM, MAX_BUFFER_DIM));
    assert_eq!(config.scale, MAX_SCALE);

    let zero = PreemWidget::Gauge {
        config: GaugeConfig {
            cols: 0,
            rows: 0,
            scale: 0,
            divisions: 0,
            subdivisions: 0,
            ..GaugeConfig::default()
        },
        state: GaugeState::default(),
    }
    .clamped();
    let PreemWidget::Gauge { config, .. } = zero else {
        panic!("variant changed")
    };
    assert_eq!((config.cols, config.rows, config.scale), (1, 1, 1));
    assert_eq!((config.divisions, config.subdivisions), (1, 1));

    let strip = PreemWidget::LedStrip {
        config: LedStripConfig {
            leds: u32::MAX,
            ..LedStripConfig::default()
        },
        state: LedStripState::default(),
    }
    .clamped();
    let PreemWidget::LedStrip { config, .. } = strip else {
        panic!("variant changed")
    };
    assert_eq!(config.leds, MAX_LEDS);

    let board = PreemWidget::FlipBoard {
        config: FlipBoardConfig {
            cells: u32::MAX,
            glyph_px: 0,
            scale: 0,
            ..FlipBoardConfig::default()
        },
        state: FlipBoardState::default(),
    }
    .clamped();
    let PreemWidget::FlipBoard { config, .. } = board else {
        panic!("variant changed")
    };
    assert_eq!(config.cells, MAX_CELLS);
    assert_eq!(config.glyph_px, 2, "the kit's MIN_GLYPH_PX floor");
    assert_eq!(config.scale, 1);

    let marquee = PreemWidget::Marquee {
        config: MarqueeConfig {
            window_px: u32::MAX,
            gap_dots: u32::MAX,
            ..MarqueeConfig::default()
        },
        state: MarqueeState::default(),
    }
    .clamped();
    let PreemWidget::Marquee { config, .. } = marquee else {
        panic!("variant changed")
    };
    assert_eq!(config.window_px, MAX_BUFFER_DIM);
    assert_eq!(config.gap_dots, MAX_BUFFER_DIM);

    let boxed = PreemWidget::TextBox {
        config: TextBoxConfig {
            width: TextBoxWidth::Cols(0),
            max_lines: 0,
            scale: 0,
            ..TextBoxConfig::default()
        },
        state: TextBoxState::default(),
    }
    .clamped();
    let PreemWidget::TextBox { config, .. } = boxed else {
        panic!("variant changed")
    };
    assert_eq!(config.width, TextBoxWidth::Cols(1));
    assert_eq!((config.max_lines, config.scale), (1, 1));
}

/// Clamping an already-legal widget is the identity — the caps must not quietly
/// rewrite ordinary values.
#[test]
fn clamping_a_legal_widget_changes_nothing() {
    for widget in all_widgets() {
        assert_eq!(
            widget.clone().clamped(),
            widget,
            "{} was mutated by clamped() despite being in range",
            widget.kind()
        );
    }
}

// ── 3. encoding stability ───────────────────────────────────────────────────

/// `Node::Label { id: None, text: "hi", classes: [] }` as a full [`encode`]
/// frame (4-byte length prefix included).
///
/// Recorded by running the *pre-change* crate — `git archive origin/main
/// crates/hytte-plugin-proto` at 7a44a2f, built standalone — so these bytes
/// predate [`Node::Preem`] rather than being whatever today's encoder happens
/// to emit.
const FROZEN_LABEL_HEX: &str = "0000001d81a54c6162656c83a26964c0a474657874a26869a7636c617373657390";

/// `Node::Pixels { id: Some("lcd"), 2×2, scale: 2 }` — the same recording, and
/// the compat-critical one: `Pixels` is what a preem-capable plugin falls back
/// to against a shell that does not advertise the preem vocabulary, so its bytes
/// must stay *exactly* what a pre-#882 host decodes.
const FROZEN_PIXELS_HEX: &str = "0000004d81a6506978656c7386a26964a36c6364a577696474680\
     2a668656967687402a464617461c410000102030405060708090a0b0c0d0e0fa57363616c650\
     2a7636c617373657391a674732d6c6364";

/// Appending [`Node::Preem`] must leave every existing variant's encoding
/// byte-identical.
///
/// This is not a theoretical worry. The whole additive-evolution story rests on
/// the serde enum representation staying **externally tagged** — a single-key
/// map keyed by the variant *name*. Switch it to internal, adjacent, or (worst)
/// untagged, or reach for a numeric discriminant, and every deployed plugin
/// binary breaks at once while the round-trip tests above stay green, because
/// both sides of a round-trip re-encode with the new scheme. Pinning literal
/// bytes recorded before the change is the only check that can see it.
///
/// Both directions are asserted per entry: today's encoder still produces the
/// frozen bytes, **and** the frozen bytes still decode to the same value.
#[test]
fn existing_node_encodings_are_frozen() {
    let cases: [(&str, &str, Node); 2] = [
        (
            "Label",
            FROZEN_LABEL_HEX,
            Node::Label {
                id: None,
                text: "hi".into(),
                classes: vec![],
            },
        ),
        (
            "Pixels",
            FROZEN_PIXELS_HEX,
            Node::Pixels {
                id: Some("lcd".into()),
                width: 2,
                height: 2,
                data: (0u8..16).collect(),
                scale: 2,
                classes: vec!["ts-lcd".into()],
            },
        ),
    ];

    for (name, frozen_hex, node) in cases {
        let frozen = from_hex(&frozen_hex.replace(['\n', ' '], ""));
        assert_eq!(
            to_hex(&encode(&node)),
            to_hex(&frozen),
            "Node::{name}'s encoding drifted — adding a variant must never move an \
             existing one's bytes. If this failed after a serde/rmp-serde change, the \
             enum representation is no longer externally (name-)tagged and every \
             deployed plugin is broken."
        );
        assert_eq!(
            decode::<Node>(&frozen).expect("frozen bytes decode"),
            node,
            "the frozen Node::{name} bytes no longer decode to the same value"
        );
    }
}

/// Regenerate the hex literals above. Ignored, and prints rather than asserts —
/// run it only for a **deliberate** wire change, and never to make a red
/// [`existing_node_encodings_are_frozen`] pass.
#[test]
#[ignore = "prints the frozen-hex literals; run explicitly, never in CI"]
fn print_frozen_node_hex() {
    println!(
        "FROZEN_LABEL_HEX  = {}",
        to_hex(&encode(&Node::Label {
            id: None,
            text: "hi".into(),
            classes: vec![],
        }))
    );
    println!(
        "FROZEN_PIXELS_HEX = {}",
        to_hex(&encode(&Node::Pixels {
            id: Some("lcd".into()),
            width: 2,
            height: 2,
            data: (0u8..16).collect(),
            scale: 2,
            classes: vec!["ts-lcd".into()],
        }))
    );
}

// ── 4. negotiation ──────────────────────────────────────────────────────────

/// The two counters mean different things and must not be collapsed: the census
/// has moved to generation 2, the *unconditional* ceiling has not.
#[test]
fn preem_is_a_negotiated_generation_not_an_unconditional_one() {
    assert_eq!(PREEM_VOCAB, 2, "the preem vocabulary is generation 2");
    assert_eq!(VOCAB, PREEM_VOCAB, "the census counts it");
    assert_eq!(
        VOCAB_UNCONDITIONAL, 1,
        "…but a plugin must not emit Node::Preem without an advertisement, so the \
         unconditional ceiling stays where it was. Raising this would make every \
         rebuilt plugin fail an older shell's check_vocab instead of degrading."
    );
    const { assert!(VOCAB_UNCONDITIONAL <= VOCAB) }
}

/// A plugin built against this proto still clears an **older** host's
/// `check_vocab` — the whole point of the split. Simulated by checking the
/// number a pre-#882 host would compare against (`VOCAB` was 1 there).
#[test]
fn a_new_plugin_clears_an_old_hosts_vocab_check() {
    /// What a pre-#882 host's own `VOCAB` was, i.e. the number it would
    /// exact-check a registering plugin against.
    const OLD_HOST_VOCAB: u16 = 1;

    let manifest = Manifest::new("preem-plugin", Mount::SidebarTop);
    assert_eq!(manifest.vocab, VOCAB_UNCONDITIONAL);
    assert_eq!(manifest.vocab_max, Some(VOCAB));

    assert!(
        manifest.vocab <= OLD_HOST_VOCAB,
        "a preem-capable plugin must not be refused by a pre-#882 shell"
    );
    // …and it correctly concludes it may not speak preem there.
    assert!(manifest.negotiated_vocab(OLD_HOST_VOCAB) < PREEM_VOCAB);

    // The host's own check still passes on this host too.
    manifest.check_vocab().expect("clears this host's check");
}

/// The full compat matrix from the draft, as arithmetic on the two declarations.
#[test]
fn negotiated_vocab_matrix() {
    let new_plugin = Manifest::new("new", Mount::BarRight);

    let mut old_plugin = Manifest::new("old", Mount::BarRight);
    old_plugin.vocab = 1;
    old_plugin.vocab_max = None; // pre-#882: the field did not exist

    // new plugin + new host → preem
    assert_eq!(new_plugin.negotiated_vocab(VOCAB), PREEM_VOCAB);
    assert!(new_plugin.negotiates_vocab());

    // new plugin + old host → falls back to Pixels
    assert!(new_plugin.negotiated_vocab(1) < PREEM_VOCAB);

    // old plugin + new host → never asks, never told, renders Pixels
    assert!(!old_plugin.negotiates_vocab());
    assert!(old_plugin.negotiated_vocab(VOCAB) < PREEM_VOCAB);

    // old plugin + old host → unchanged
    assert!(old_plugin.negotiated_vocab(1) < PREEM_VOCAB);

    // A host advertising *beyond* what the plugin can speak never lifts it past
    // its own ceiling.
    assert_eq!(new_plugin.negotiated_vocab(u16::MAX), VOCAB);
}

/// A pre-#882 manifest — one whose encoding has no `vocab_max` key at all —
/// still decodes, to the non-negotiating default. This is the actual on-the-wire
/// backward direction, not a hand-set field.
#[test]
fn a_manifest_without_vocab_max_decodes_as_non_negotiating() {
    let legacy = rmp_serde::to_vec_named(&LegacyManifest {
        id: "legacy".into(),
        proto: hytte_plugin_proto::PROTO_VERSION,
        vocab: 1,
        subscribes: vec![],
        capabilities: vec![],
        mount: Mount::BarLeft,
    })
    .expect("legacy manifest encodes");

    let back = decode_body::<Manifest>(&legacy).expect("a pre-#882 manifest still decodes");
    assert_eq!(back.vocab_max, None);
    assert!(!back.negotiates_vocab());
    back.check_vocab().expect("and still clears the check");
}

/// A non-negotiating manifest stays **byte-identical** to a pre-#882 one:
/// `vocab_max: None` carries `skip_serializing_if`, so the new field costs a
/// legacy plugin nothing on the wire.
#[test]
fn a_non_negotiating_manifest_is_byte_identical_to_a_legacy_one() {
    let mut manifest = Manifest::new("legacy", Mount::BarLeft);
    manifest.vocab = 1;
    manifest.vocab_max = None;

    let legacy = rmp_serde::to_vec_named(&LegacyManifest {
        id: "legacy".into(),
        proto: hytte_plugin_proto::PROTO_VERSION,
        vocab: 1,
        subscribes: vec![],
        capabilities: vec![],
        mount: Mount::BarLeft,
    })
    .expect("legacy manifest encodes");

    assert_eq!(
        to_hex(&encode_body(&manifest)),
        to_hex(&legacy),
        "the new vocab_max field must be invisible on the wire when unset"
    );
}

/// The advertisement frame round-trips, and a plugin resolves the same
/// generation the host would.
#[test]
fn hello_round_trips_and_drives_the_decision() {
    let msg = HostMsg::Hello { vocab: VOCAB };
    let back = decode::<HostMsg>(&encode(&msg)).expect("Hello decodes");
    assert_eq!(back, msg);

    let HostMsg::Hello { vocab } = back else {
        panic!("decoded to the wrong variant")
    };
    let manifest = Manifest::new("preem-plugin", Mount::SidebarTop);
    assert!(
        manifest.negotiated_vocab(vocab) >= PREEM_VOCAB,
        "a current plugin talking to a current host speaks preem"
    );
}

// ── local serde shapes used to synthesize "other encoder" frames ────────────

#[derive(serde::Serialize)]
struct Empty {}

#[derive(serde::Serialize)]
struct OnlyText {
    text: String,
}

#[derive(serde::Serialize)]
struct SparseBody {
    config: Empty,
    state: OnlyText,
}

/// A hand-rolled `{"TextBox": {"config": {}, "state": {"text": …}}}` — the frame
/// a minimal non-Rust encoder would emit.
#[derive(serde::Serialize)]
struct SparseTextBox {
    #[serde(rename = "TextBox")]
    text_box: SparseBody,
}

/// The `Manifest` as it was **before** #882 added `vocab_max`: same field names,
/// same order, one key fewer.
#[derive(serde::Serialize)]
struct LegacyManifest {
    id: String,
    proto: u16,
    vocab: u16,
    subscribes: Vec<hytte_plugin_proto::StateKey>,
    capabilities: Vec<hytte_plugin_proto::Capability>,
    mount: Mount,
}
