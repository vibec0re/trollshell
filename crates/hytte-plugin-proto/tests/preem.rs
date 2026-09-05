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
//!    documented cap, including on a char boundary for text, and sanitises
//!    every non-finite float so a clamped widget compares equal to itself
//!    under the derived `PartialEq` (which is what both ends' render dedup
//!    rests on — see the `clamp_in_place` docs).
//! 3. **Encoding stability** — adding [`Node::Preem`] must not have moved any
//!    *existing* node's bytes. [`existing_node_encodings_are_frozen`] pins two
//!    of them against hex literals recorded before the variant existed, so an
//!    accidental switch away from external (name-keyed) enum tagging fails loud
//!    here rather than silently bricking every deployed plugin.

use hytte_plugin_proto::{
    AccentRole, Cls, DotMatrixConfig, DotMatrixState, FlipBoardConfig, FlipBoardState, GaugeConfig,
    GaugeRange, GaugeState, HostMsg, LedStripConfig, LedStripState, MAX_BUFFER_DIM, MAX_CELLS,
    MAX_DAMPING, MAX_FLIP_DURATION_SECS, MAX_FLIP_STAGGER_SECS, MAX_FREQUENCY_HZ, MAX_GAP_DOTS,
    MAX_LEDS, MAX_MARQUEE_SPEED_DPS, MAX_PEAK_HOLD_RATE, MAX_RASTER_PIXELS, MAX_SCALE,
    MAX_SCOPE_SAMPLES, MAX_STRIP_DIM, MAX_SWEEP_DEG, MAX_TEXT_LEN, MIN_DAMPING,
    MIN_FLIP_DURATION_SECS, MIN_FREQUENCY_HZ, MIN_SWEEP_DEG, Manifest, MarqueeConfig, MarqueeState,
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
        assert_eq!(
            back,
            node,
            "Node::Preem({}) did not round-trip",
            widget.kind()
        );
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
            widget: Box::new(widget.clone()),
        }
    );
    assert_eq!(
        preem_id("clock", widget.clone()),
        Node::Preem {
            id: Some("clock".into()),
            classes: vec![],
            widget: Box::new(widget.clone()),
        }
    );
    assert_eq!(
        preem_styled("clock", vec!["a".into()], widget.clone()),
        Node::Preem {
            id: Some("clock".into()),
            classes: vec!["a".into()],
            widget: Box::new(widget),
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

/// The same guarantee for the two **nested** config structs, which are the ones
/// it used to be false for: without a container-level `#[serde(default)]` on
/// [`GaugeRange`] and [`PeakHoldConfig`], a partial `range` (only `low`) or a
/// `peak_hold` with no `rate` was a hard decode failure while the module doc
/// promised every omitted field defaults. Doc and code now agree.
#[test]
fn omitted_nested_config_keys_decode_to_defaults() {
    // {"Gauge": {"config": {"range": {"low": -20.0}}, "state": {}}} — `high`
    // absent, and every other config key absent too.
    let partial_range = rmp_serde::to_vec_named(&SparseGauge {
        gauge: SparseGaugeBody {
            config: OnlyRange {
                range: OnlyLow { low: -20.0 },
            },
            state: Empty {},
        },
    })
    .expect("sparse gauge encodes");

    let back = decode_body::<PreemWidget>(&partial_range).expect("a partial range still decodes");
    assert_eq!(
        back,
        PreemWidget::Gauge {
            config: GaugeConfig {
                range: GaugeRange {
                    low: -20.0,
                    high: 1.0, // the default, not a decode error
                },
                ..GaugeConfig::default()
            },
            state: GaugeState::default(),
        }
    );

    // {"LedStrip": {"config": {"peak_hold": {}}, "state": {}}} — `rate` absent.
    let bare_peak_hold = rmp_serde::to_vec_named(&SparseStrip {
        led_strip: SparseStripBody {
            config: OnlyPeakHold {
                peak_hold: Empty {},
            },
            state: Empty {},
        },
    })
    .expect("sparse strip encodes");

    let back =
        decode_body::<PreemWidget>(&bare_peak_hold).expect("a rate-less peak_hold still decodes");
    assert_eq!(
        back,
        PreemWidget::LedStrip {
            config: LedStripConfig {
                peak_hold: Some(PeakHoldConfig { rate: 0.0 }),
                ..LedStripConfig::default()
            },
            state: LedStripState::default(),
        }
    );
}

/// The one compat cell the rest of this suite doesn't cover: an **old decoder
/// meeting a new frame**.
///
/// Everything else here proves new-decodes-old. This proves the other
/// direction fails *cleanly* — `Node::Preem` and `HostMsg::Hello` hit a
/// pre-#882 enum shape as an `unknown variant` error, not as a misparse into
/// some neighbouring variant. That distinction is what makes the negotiation
/// safe to reason about: a mis-behaved plugin that emits `Preem` past a
/// non-advertising host produces one loud decode failure, never a silently
/// wrong widget.
#[test]
fn a_pre_882_decoder_rejects_the_new_variants_cleanly() {
    let preem_frame = encode_body(&preem_id("g", all_widgets()[0].clone()));
    let err = rmp_serde::from_slice::<LegacyNode>(&preem_frame)
        .expect_err("a pre-#882 Node must not decode a Preem frame");
    let msg = err.to_string();
    assert!(
        msg.contains("unknown variant") && msg.contains("Preem"),
        "expected a clean unknown-variant error naming Preem, got: {msg}"
    );

    let hello_frame = encode_body(&HostMsg::Hello { vocab: VOCAB });
    let err = rmp_serde::from_slice::<LegacyHostMsg>(&hello_frame)
        .expect_err("a pre-#882 HostMsg must not decode a Hello frame");
    let msg = err.to_string();
    assert!(
        msg.contains("unknown variant") && msg.contains("Hello"),
        "expected a clean unknown-variant error naming Hello, got: {msg}"
    );

    // …and the same decoder still reads a frame it *does* know, so the failures
    // above are about the new variants, not a broken mirror.
    let label = encode_body(&Node::Label {
        id: None,
        text: "hi".into(),
        classes: vec![],
    });
    rmp_serde::from_slice::<LegacyNode>(&label).expect("the legacy mirror still decodes a Label");
}

/// Pins the **wire** defaults against the values this module's docs claim the
/// kit uses.
///
/// Named for what it can actually check. It cannot see kit drift: this crate is
/// the language-neutral schema anchor and deliberately does not depend on
/// `hytte-preem`, so if someone changes `TextBox::new`'s 16 columns tomorrow,
/// this test stays green and the *doc* becomes the lie. The citations on each
/// case are the manual link. A real cross-check has to live on the kit side
/// (a `hytte-preem` test asserting its constructors against these numbers) —
/// out of lane for a proto-only PR; noted as a #884-era follow-up.
#[test]
fn wire_defaults_match_the_documented_kit_values() {
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

// ── the explicit ink pin (#885) ─────────────────────────────────────────────

/// Pin `ink` into a widget's style reference, whatever variant it is — the
/// mutable twin of [`PreemWidget::style`], which is read-only.
fn pin_ink(widget: &mut PreemWidget, ink: hytte_plugin_proto::preem::Rgba) {
    let style = match widget {
        PreemWidget::DotMatrix { config, .. } => &mut config.style,
        PreemWidget::SevenSeg { config, .. } => &mut config.style,
        PreemWidget::TextBox { config, .. } => &mut config.style,
        PreemWidget::LedStrip { config, .. } => &mut config.style,
        PreemWidget::Marquee { config, .. } => &mut config.style,
        PreemWidget::Scope { config, .. } => &mut config.style,
        PreemWidget::Gauge { config, .. } => &mut config.style,
        PreemWidget::FlipBoard { config, .. } => &mut config.style,
    };
    style.ink = Some(ink);
}

/// A frame written by a peer that predates [`StyleRef::ink`] — the field simply
/// absent from the map — decodes with no pin, and so renders exactly as it did
/// before the field existed.
///
/// This is half of the compatible-addition claim (#895's vocabulary is
/// *unbumped* by this change): the wire is a named-field map
/// (`rmp_serde::to_vec_named`), so an unknown key is skippable and a missing one
/// falls to `#[serde(default)]`.
///
/// **Falsified** by giving `ink` a `#[serde(default = …)]` that returns a color:
/// the decode then invents a pin nobody asked for and both assertions go red.
///
/// Worth knowing which mutation *doesn't* falsify it, since it is the obvious
/// guess: dropping the container's `#[serde(default)]` leaves this green,
/// because serde's `missing_field` helper can already produce `None` for an
/// `Option` field on its own. The absent-key guarantee for `ink` therefore rests
/// on the field being an `Option` at all — not on the container attribute, which
/// is what carries the *other* keys.
#[test]
fn a_style_ref_written_before_the_ink_field_decodes_unpinned() {
    for (pre, want) in [
        (
            PreInkStyleRef {
                style: StyleName::Lcd,
                accent: Some(AccentRole::Warning),
            },
            StyleRef::new(StyleName::Lcd).with_accent(AccentRole::Warning),
        ),
        (
            PreInkStyleRef {
                style: StyleName::Vfd,
                accent: None,
            },
            StyleRef::new(StyleName::Vfd),
        ),
    ] {
        let bytes = rmp_serde::to_vec_named(&pre).expect("the pre-#885 shape encodes");
        let back = decode_body::<StyleRef>(&bytes).expect("a pre-#885 style ref still decodes");
        assert_eq!(back, want);
        assert_eq!(back.ink, None, "an absent key is not a pin");
    }
}

/// The other half: a style reference with **no** pin encodes to exactly the
/// bytes the pre-#885 shape does, so a plugin built against the old proto and a
/// shell built against the new one (or the reverse) exchange identical frames.
///
/// The repo's byte-pinned `plugin_render_preem_v1` golden fixture is the
/// belt-and-braces version of this claim — it is committed to git and was **not**
/// regenerated for this change.
///
/// **Falsified** by dropping `skip_serializing_if` from `StyleRef::ink`: the
/// unpinned form starts writing an explicit `ink: nil` and both assertions go
/// red.
#[test]
fn an_unpinned_style_ref_is_byte_identical_to_the_pre_change_form() {
    let cases = [
        (
            StyleRef::new(StyleName::Lcd).with_accent(AccentRole::Warning),
            PreInkStyleRef {
                style: StyleName::Lcd,
                accent: Some(AccentRole::Warning),
            },
        ),
        (
            StyleRef::new(StyleName::Vfd),
            PreInkStyleRef {
                style: StyleName::Vfd,
                accent: None,
            },
        ),
    ];
    for (now, before) in cases {
        assert_eq!(
            encode_body(&now),
            rmp_serde::to_vec_named(&before).expect("the pre-#885 shape encodes"),
            "an unpinned {now:?} must not add a byte to the wire"
        );
    }

    // …and the same claim where it actually matters: inside a whole widget, the
    // key must not appear at all.
    //
    // Matched as the encoded *key* (`0xa3` = MessagePack fixstr of length 3,
    // then `ink`) rather than as the substring "ink" anywhere in the frame: a
    // widget whose state text happened to contain "PINK" or "thinking" would
    // trip a loose scan. It could only ever false-*fail*, never false-pass, but
    // a test that a future fixture can break for the wrong reason is worth four
    // more bytes of precision.
    let ink_key: &[u8] = b"\xa3ink";
    let carries_key =
        |widget: &PreemWidget| encode(widget).windows(ink_key.len()).any(|w| w == ink_key);
    for widget in all_widgets() {
        assert!(
            !carries_key(&widget),
            "{}'s unpinned encoding must not carry an `ink` key",
            widget.kind()
        );
        // The positive control: an anchored needle that never matches anything
        // would satisfy the assertion above for the wrong reason. Pin the same
        // widget and the key must appear.
        let mut pinned = widget.clone();
        pin_ink(&mut pinned, [0x11, 0x22, 0x33, 0xff]);
        assert!(
            carries_key(&pinned),
            "{}'s pinned encoding must carry exactly that key, or the absence check above is blind",
            widget.kind()
        );
    }
}

/// A pin does travel, survives the codec, and reaches the far side as the exact
/// color that was set — the claim the shell's "this widget is excluded from the
/// re-tint" behavior rests on.
#[test]
fn a_pinned_ink_travels_on_the_wire() {
    let violet = [0x9b, 0x59, 0xb6, 0xff];
    for widget in all_widgets() {
        let mut pinned = widget.clone();
        pin_ink(&mut pinned, violet);
        assert_ne!(pinned, widget, "pinning changed the widget");

        let back = decode::<PreemWidget>(&encode(&pinned)).expect("a pinned widget decodes");
        assert_eq!(back, pinned, "{} did not round-trip pinned", widget.kind());
        assert_eq!(
            back.style().ink,
            Some(violet),
            "{} lost its pinned ink",
            widget.kind()
        );
    }
}

/// [`PreemWidget::clamped`] has nothing to enforce on a pin: four `u8`s are four
/// `u8`s, every bit pattern is a color, and there is no non-finite case to fold.
/// So the pin survives clamping byte for byte, and reflexivity — the property
/// #899 exists to protect — still holds with one present.
///
/// Includes the two values a sanitiser would be tempted to "fix": fully
/// transparent, and pure black.
///
/// **Falsified** by having `clamp_in_place` clear or normalize `style.ink`: the
/// "clamping is not allowed to touch a pin" assertion goes red.
#[test]
fn clamping_leaves_a_pinned_ink_untouched_and_stays_reflexive() {
    for ink in [
        [0x9b, 0x59, 0xb6, 0xff],
        [0x00, 0x00, 0x00, 0x00],
        [0x00, 0x00, 0x00, 0xff],
        [0xff, 0xff, 0xff, 0xff],
    ] {
        for widget in all_widgets() {
            let mut pinned = widget.clone();
            pin_ink(&mut pinned, ink);
            let clamped = pinned.clone().clamped();
            assert_eq!(
                clamped.style().ink,
                Some(ink),
                "{}: clamping is not allowed to touch a pin",
                widget.kind()
            );
            assert_eq!(
                clamped,
                pinned.clamped(),
                "{}: clamped(w) == clamped(w) with a pin present",
                widget.kind()
            );
        }
    }
}

// ── 2. wire limits ──────────────────────────────────────────────────────────

/// The text cap is enforced, and enforced on a **char boundary**: a naive
/// `String::truncate` at [`MAX_TEXT_LEN`] would panic mid-codepoint, which is
/// precisely the "malformed input must never crash the shell" failure this cap
/// exists to prevent.
#[test]
fn text_cap_truncates_on_a_char_boundary() {
    // 3-byte chars, so the cap lands *inside* a codepoint whenever
    // MAX_TEXT_LEN isn't a multiple of 3 (it isn't: 2048 = 3*682 + 2).
    let whole_chars = MAX_TEXT_LEN / 3;
    let want_len = whole_chars * 3;
    assert_ne!(
        want_len, MAX_TEXT_LEN,
        "this test needs a cap that splits a 3-byte codepoint"
    );
    let long = "☃".repeat(MAX_TEXT_LEN);
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
    assert_eq!(state.text.len(), want_len, "must cut at the char boundary");
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
#[allow(clippy::too_many_lines)]
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
    // Not MAX_SCALE: the buffer is already at the dimension cap, so the scaled
    // dimension rule pulls the upscale all the way down to 1. This is the whole
    // point of the B1 fix — capping the dimension and the multiplier
    // independently would have allowed 2048×8 = 16384 px per axis.
    assert_eq!(
        config.scale, 1,
        "a max-dimension buffer must not also get a max upscale"
    );

    // …and a *small* buffer still gets the full upscale, so the rule bounds the
    // product without flattening ordinary configs.
    let small = PreemWidget::Scope {
        config: ScopeConfig {
            cols: 16,
            rows: 16,
            scale: u32::MAX,
            ..ScopeConfig::default()
        },
        state: ScopeState::default(),
    }
    .clamped();
    let PreemWidget::Scope { config, .. } = small else {
        panic!("variant changed")
    };
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
    assert_eq!(config.gap_dots, MAX_GAP_DOTS);

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

/// **The B1 invariant**: after [`PreemWidget::clamped`], no widget — at *any*
/// config a plugin can put on the wire — asks the shell to rasterise more than
/// [`MAX_RASTER_PIXELS`], on either axis or in area.
///
/// Per-field caps are not the property that matters; their product is. Before
/// the fix each field was capped independently, which bounded nothing useful:
/// `cols = rows = 4096` with `scale = 8` was a *legal* config describing a
/// 32768×32768 buffer — a **4.00 GiB** allocation demand from about thirty
/// bytes of wire, an amplification of ~10⁸:1. `TextBox`'s `pad` (added to both
/// dimensions) reached ~17 GB around an empty string.
///
/// The footprints below are computed from the **kit's real geometry**
/// (`hytte-preem`'s font metrics and cell pitches, cited per case) rather than
/// from the proto's own estimate constants, so a wrong estimate in
/// `fit_scale`'s helpers fails here instead of silently under-bounding.
// A worst-case case per widget; the length is the vocabulary size.
#[allow(clippy::too_many_lines)]
#[test]
fn preem_worst_case_footprint_is_bounded() {
    // Kit geometry (crates/hytte-preem/src/*), mirrored here so this test is an
    // independent check of the proto's caps rather than a restatement of them.
    const GLYPH_W: u32 = 5; // font.rs:31
    const GLYPH_H: u32 = 7; // font.rs:33
    const SPACING: u32 = 1; // font.rs:35
    const LINE_GAP: u32 = 2; // font.rs:37
    // The dot-matrix "virtual pixel": every font pixel is a DOT×DOT dot.
    const DOT: u32 = 4; // dot_matrix.rs:31
    const DOT_PAD: u32 = 4; // dot_matrix.rs:34 (= DOT)
    // Seven-segment metrics — its own grid, unrelated to the font's.
    const SEG_DIGIT_W: u32 = 30; // seven_seg.rs:26
    const SEG_DIGIT_H: u32 = 54; // seven_seg.rs:28
    const SEG_GAP: u32 = 10; // seven_seg.rs:32
    const SEG_PAD: u32 = 8; // seven_seg.rs:34
    const STRIP_PAD: u32 = 4; // led_strip.rs:45
    const STRIP_CELL_W: u32 = 8; // led_strip.rs:39
    const STRIP_CELL_H: u32 = 16; // led_strip.rs:41
    const STRIP_GAP: u32 = 3; // led_strip.rs:43

    /// `(final_width, final_height)` a clamped config would rasterise to.
    fn footprint(w: &PreemWidget) -> (u32, u32) {
        match w {
            // **The trap, and why this arm is spelled out.** A dot matrix does
            // NOT render one buffer pixel per font pixel: every font pixel
            // becomes a `DOT`×`DOT` round dot, so a char cell advances
            // `(GLYPH_W + SPACING) * DOT` = **24 px**, not 6
            // (dot_matrix.rs:30-31 and :62-68). Reasoning at the bare font
            // pitch under-counts the width by 4× — which is exactly the error
            // that shipped in the first version of this test, where both strip
            // arms were `(chars * 6, 7)` and so validated the model against
            // itself.
            PreemWidget::DotMatrix { state, .. } => {
                let n = u32::try_from(state.text.chars().count()).unwrap_or(u32::MAX);
                let advance = (GLYPH_W + SPACING) * DOT; // 24
                let w = if n == 0 {
                    2 * DOT_PAD
                } else {
                    2 * DOT_PAD + n * advance - SPACING * DOT // 24n + 4
                };
                (w, 2 * DOT_PAD + GLYPH_H * DOT) // 36 high
            }
            // A seven-segment readout shares *nothing* with the font grid — it
            // has its own cell metrics entirely (seven_seg.rs:24-34). The
            // widest cell is a digit, so the worst-case pitch is
            // `DIGIT_W + GAP` = 40 px/char and the width is `40n + 6`.
            PreemWidget::SevenSeg { state, .. } => {
                let n = u32::try_from(state.text.chars().count()).unwrap_or(u32::MAX);
                let w = if n == 0 {
                    2 * SEG_PAD
                } else {
                    2 * SEG_PAD + n * (SEG_DIGIT_W + SEG_GAP) - SEG_GAP // 40n + 6
                };
                (w, 2 * SEG_PAD + SEG_DIGIT_H) // 70 high
            }
            PreemWidget::TextBox { config, state: _ } => {
                let cols = match config.width {
                    TextBoxWidth::Cols(n) => n,
                    // A FitPx budget resolves to at most that many pixels.
                    TextBoxWidth::FitPx(px) => px / (GLYPH_W + SPACING),
                };
                let w = (2 * config.pad + cols * (GLYPH_W + SPACING)) * config.scale;
                let h = (2 * config.pad
                    + config.max_lines * GLYPH_H
                    + config.max_lines.saturating_sub(1) * LINE_GAP)
                    * config.scale;
                (w, h)
            }
            PreemWidget::LedStrip { config, .. } => (
                2 * STRIP_PAD
                    + config.leds * STRIP_CELL_W
                    + config.leds.saturating_sub(1) * STRIP_GAP,
                2 * STRIP_PAD + STRIP_CELL_H,
            ),
            // window_px is already the final width; the height is fixed.
            PreemWidget::Marquee { config, .. } => (config.window_px, 2 * 2 + GLYPH_H * 2),
            PreemWidget::Scope { config, .. } => {
                (config.cols * config.scale, config.rows * config.scale)
            }
            PreemWidget::Gauge { config, .. } => {
                (config.cols * config.scale, config.rows * config.scale)
            }
            PreemWidget::FlipBoard { config, .. } => {
                // (glyph + 2*card pad + inter-card gap) per cell, + bezel, all
                // in font pixels, times glyph_px times scale (split_flap.rs).
                let per_cell = GLYPH_W + 2 + 1;
                let w = (config.cells * per_cell + 2) * config.glyph_px * config.scale;
                let h = (GLYPH_H + 4) * config.glyph_px * config.scale;
                (w, h)
            }
        }
    }

    // Every knob pinned to its most expensive legal value.
    let worst: Vec<PreemWidget> = vec![
        PreemWidget::DotMatrix {
            config: DotMatrixConfig::default(),
            state: DotMatrixState {
                text: "8".repeat(MAX_TEXT_LEN * 4),
            },
        },
        // Was missing entirely, which is half of why the seven-seg overflow
        // went unseen: an unexercised variant can't fail a bound.
        PreemWidget::SevenSeg {
            config: SevenSegConfig::default(),
            state: SevenSegState {
                text: "8".repeat(MAX_TEXT_LEN * 4),
            },
        },
        PreemWidget::TextBox {
            config: TextBoxConfig {
                width: TextBoxWidth::Cols(u32::MAX),
                max_lines: u32::MAX,
                pad: u32::MAX,
                corner: u32::MAX,
                scale: u32::MAX,
                ..TextBoxConfig::default()
            },
            state: TextBoxState {
                text: "x".repeat(MAX_TEXT_LEN * 4),
            },
        },
        PreemWidget::LedStrip {
            config: LedStripConfig {
                leds: u32::MAX,
                ..LedStripConfig::default()
            },
            state: LedStripState::default(),
        },
        PreemWidget::Marquee {
            config: MarqueeConfig {
                window_px: u32::MAX,
                gap_dots: u32::MAX,
                speed_dots_per_sec: f32::INFINITY,
                ..MarqueeConfig::default()
            },
            state: MarqueeState::default(),
        },
        PreemWidget::Scope {
            config: ScopeConfig {
                cols: u32::MAX,
                rows: u32::MAX,
                scale: u32::MAX,
                ..ScopeConfig::default()
            },
            state: ScopeState::default(),
        },
        PreemWidget::Gauge {
            config: GaugeConfig {
                cols: u32::MAX,
                rows: u32::MAX,
                scale: u32::MAX,
                divisions: u32::MAX,
                subdivisions: u32::MAX,
                ..GaugeConfig::default()
            },
            state: GaugeState::default(),
        },
        PreemWidget::FlipBoard {
            config: FlipBoardConfig {
                cells: u32::MAX,
                glyph_px: u32::MAX,
                scale: u32::MAX,
                ..FlipBoardConfig::default()
            },
            state: FlipBoardState::default(),
        },
        // The *narrow* board, which is the only shape where a flip board is
        // taller than it is wide (10 font-px across, 11 down). Without it the
        // wide case above passes on width alone and the height axis is never
        // exercised — the fit would be correct only by aspect ratio.
        PreemWidget::FlipBoard {
            config: FlipBoardConfig {
                cells: 1,
                glyph_px: u32::MAX,
                scale: u32::MAX,
                ..FlipBoardConfig::default()
            },
            state: FlipBoardState::default(),
        },
    ];

    let mut violations: Vec<String> = Vec::new();
    for widget in worst {
        let kind = widget.kind();
        // A single-line strip is a few px tall and as wide as its message, so
        // the square-ish MAX_BUFFER_DIM is the wrong per-axis rule for it; what
        // binds there is texture upload (MAX_STRIP_DIM). Area binds everything.
        //
        // Exactly the two text strips — matching MAX_STRIP_DIM's own doc.
        // `LedStrip` used to be listed here and never needed it (its real
        // worst case is 1413 px, inside MAX_BUFFER_DIM), so the exemption was
        // dead code that quietly widened what this test would accept.
        // `Marquee` never needed it either: the kit allocates only the window
        // (marquee.rs:190), so `window_px` alone bounds it.
        let is_strip = matches!(
            widget,
            PreemWidget::DotMatrix { .. } | PreemWidget::SevenSeg { .. }
        );
        let clamped = widget.clamped();
        let (w, h) = footprint(&clamped);
        let area = u64::from(w) * u64::from(h);

        let axis_cap = if is_strip {
            MAX_STRIP_DIM
        } else {
            MAX_BUFFER_DIM
        };
        // Collected rather than asserted per-iteration: a per-widget `assert!`
        // aborts at the first violation, so a second unbounded widget stays
        // invisible until the first is fixed. Report every one.
        if w > axis_cap || h > axis_cap {
            violations.push(format!(
                "{kind}: footprint {w}×{h} exceeds its per-axis cap ({axis_cap})"
            ));
        }
        if area > u64::from(MAX_RASTER_PIXELS) {
            // Ratio in integer hundredths — `u64 as f64` loses precision above
            // 2^53 and the lint is right to say so.
            let hundredths = area * 100 / u64::from(MAX_RASTER_PIXELS);
            violations.push(format!(
                "{kind}: area {area} px exceeds MAX_RASTER_PIXELS ({MAX_RASTER_PIXELS}) \
                 — {}.{:02}× over",
                hundredths / 100,
                hundredths % 100
            ));
        }
    }

    assert!(
        violations.is_empty(),
        "the caps bound the fields but not the buffer:\n  {}",
        violations.join("\n  ")
    );
}

/// The gauge's per-frame *CPU* cost is bounded too — no buffer cap can see it,
/// because `divisions * subdivisions` ticks are rasterised at any buffer size
/// (`gauge.rs`'s `tick_marks`). Independently capped 4096s would be 16.7M line
/// draws per frame.
#[test]
fn gauge_tick_count_is_bounded() {
    let clamped = PreemWidget::Gauge {
        config: GaugeConfig {
            divisions: u32::MAX,
            subdivisions: u32::MAX,
            ..GaugeConfig::default()
        },
        state: GaugeState::default(),
    }
    .clamped();
    let PreemWidget::Gauge { config, .. } = clamped else {
        panic!("variant changed")
    };
    let ticks = u64::from(config.divisions) * u64::from(config.subdivisions);
    assert!(
        ticks <= 2048,
        "gauge would rasterise {ticks} ticks per frame"
    );
}

/// `speed_dots_per_sec` is the one float with no kit clamp behind it, so
/// `clamped()` owns it: a non-finite speed parks the marquee rather than
/// poisoning the shell's offset integrator, and the magnitude is capped.
#[test]
fn marquee_speed_is_clamped_including_non_finite() {
    for (input, want) in [
        (f32::NAN, 0.0_f32),
        (f32::INFINITY, 0.0),
        (f32::NEG_INFINITY, 0.0),
        (1.0e9, MAX_MARQUEE_SPEED_DPS),
        (-1.0e9, -MAX_MARQUEE_SPEED_DPS),
        (20.0, 20.0),
    ] {
        let clamped = PreemWidget::Marquee {
            config: MarqueeConfig {
                speed_dots_per_sec: input,
                ..MarqueeConfig::default()
            },
            state: MarqueeState::default(),
        }
        .clamped();
        let PreemWidget::Marquee { config, .. } = clamped else {
            panic!("variant changed")
        };
        assert!(
            (config.speed_dots_per_sec - want).abs() < f32::EPSILON,
            "speed {input} clamped to {}, want {want}",
            config.speed_dots_per_sec
        );
    }
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

// ── 2b. non-finite floats ───────────────────────────────────────────────────

/// Every widget kind with **every** `f32` it carries set to `poison`, and every
/// non-float field left legal — so a failure below can only be about floats.
///
/// The three text widgets carry no float at all; they are in the list so the
/// per-kind loops stay honest about covering all eight rather than quietly
/// skipping the ones that would be awkward.
fn widgets_with_every_float_poisoned(poison: f32) -> Vec<PreemWidget> {
    vec![
        PreemWidget::DotMatrix {
            config: DotMatrixConfig::default(),
            state: DotMatrixState {
                text: "12:34".into(),
            },
        },
        PreemWidget::SevenSeg {
            config: SevenSegConfig::default(),
            state: SevenSegState {
                text: "88.8".into(),
            },
        },
        PreemWidget::TextBox {
            config: TextBoxConfig::default(),
            state: TextBoxState {
                text: "no floats here".into(),
            },
        },
        PreemWidget::LedStrip {
            config: LedStripConfig {
                peak_hold: Some(PeakHoldConfig { rate: poison }),
                ..LedStripConfig::default()
            },
            state: LedStripState {
                level: poison,
                peak: Some(poison),
            },
        },
        PreemWidget::Marquee {
            config: MarqueeConfig {
                speed_dots_per_sec: poison,
                ..MarqueeConfig::default()
            },
            state: MarqueeState {
                text: "poisoned".into(),
            },
        },
        PreemWidget::Scope {
            config: ScopeConfig::default(),
            state: ScopeState {
                samples: vec![poison, 0.5, poison, -0.5, poison],
            },
        },
        PreemWidget::Gauge {
            config: GaugeConfig {
                sweep_deg: poison,
                frequency_hz: poison,
                damping: poison,
                range: GaugeRange {
                    low: poison,
                    high: poison,
                },
                ..GaugeConfig::default()
            },
            state: GaugeState { target: poison },
        },
        PreemWidget::FlipBoard {
            config: FlipBoardConfig {
                duration_secs: Some(poison),
                stagger_secs: Some(poison),
                ..FlipBoardConfig::default()
            },
            state: FlipBoardState {
                text: "SPANDAU".into(),
            },
        },
    ]
}

/// Exact float equality by bit pattern — stricter than `==` (which would let a
/// `-0.0` → `0.0` rewrite pass) and lint-clean, unlike a bare float compare.
#[track_caller]
fn assert_bits(got: f32, want: f32, what: &str) {
    assert_eq!(
        got.to_bits(),
        want.to_bits(),
        "{what}: got {got}, want {want}"
    );
}

#[track_caller]
fn assert_in_range(value: f32, lo: f32, hi: f32, what: &str) {
    assert!(value.is_finite(), "{what} is not finite: {value}");
    assert!(
        value >= lo && value <= hi,
        "{what} = {value}, want {lo}..={hi}"
    );
}

/// Half (a) of the invariant `PreemWidget::clamp_in_place` documents: every
/// float this widget carries is finite and inside its documented bounds.
#[track_caller]
fn assert_every_float_is_sane(widget: &PreemWidget) {
    match widget {
        // No floats at all — stated rather than defaulted, so a float added to
        // one of these later fails here instead of going unchecked.
        PreemWidget::DotMatrix { .. }
        | PreemWidget::SevenSeg { .. }
        | PreemWidget::TextBox { .. } => {}
        PreemWidget::LedStrip { config, state } => {
            if let Some(hold) = config.peak_hold {
                assert_in_range(hold.rate, 0.0, MAX_PEAK_HOLD_RATE, "peak_hold.rate");
            }
            assert_in_range(state.level, 0.0, 1.0, "level");
            if let Some(peak) = state.peak {
                assert_in_range(peak, 0.0, 1.0, "peak");
            }
        }
        PreemWidget::Marquee { config, .. } => assert_in_range(
            config.speed_dots_per_sec,
            -MAX_MARQUEE_SPEED_DPS,
            MAX_MARQUEE_SPEED_DPS,
            "speed_dots_per_sec",
        ),
        PreemWidget::Scope { state, .. } => {
            for (i, sample) in state.samples.iter().enumerate() {
                assert_in_range(*sample, -1.0, 1.0, &format!("samples[{i}]"));
            }
        }
        PreemWidget::Gauge { config, state } => {
            assert_in_range(config.sweep_deg, MIN_SWEEP_DEG, MAX_SWEEP_DEG, "sweep_deg");
            assert_in_range(
                config.frequency_hz,
                MIN_FREQUENCY_HZ,
                MAX_FREQUENCY_HZ,
                "frequency_hz",
            );
            assert_in_range(config.damping, MIN_DAMPING, MAX_DAMPING, "damping");
            let GaugeRange { low, high } = config.range;
            assert!(low.is_finite() && high.is_finite(), "range {low}..={high}");
            assert!(high > low, "degenerate range {low}..={high}");
            assert!(
                (high - low).is_finite(),
                "range span overflows: {low}..={high}"
            );
            assert_in_range(state.target, low, high, "target");
        }
        PreemWidget::FlipBoard { config, .. } => {
            if let Some(duration) = config.duration_secs {
                assert_in_range(
                    duration,
                    MIN_FLIP_DURATION_SECS,
                    MAX_FLIP_DURATION_SECS,
                    "duration_secs",
                );
            }
            if let Some(stagger) = config.stagger_secs {
                assert_in_range(stagger, 0.0, MAX_FLIP_STAGGER_SECS, "stagger_secs");
            }
        }
    }
}

/// Every `f32` a widget carries, as bit patterns, in declaration order — for
/// the identity check below, which wants *exact* comparison rather than `==`.
fn float_bits(widget: &PreemWidget) -> Vec<u32> {
    match widget {
        PreemWidget::DotMatrix { .. }
        | PreemWidget::SevenSeg { .. }
        | PreemWidget::TextBox { .. } => Vec::new(),
        PreemWidget::LedStrip { config, state } => {
            let mut bits: Vec<u32> = config
                .peak_hold
                .iter()
                .map(|hold| hold.rate.to_bits())
                .collect();
            bits.push(state.level.to_bits());
            bits.extend(state.peak.iter().map(|peak| peak.to_bits()));
            bits
        }
        PreemWidget::Marquee { config, .. } => vec![config.speed_dots_per_sec.to_bits()],
        PreemWidget::Scope { state, .. } => state.samples.iter().map(|s| s.to_bits()).collect(),
        PreemWidget::Gauge { config, state } => vec![
            config.sweep_deg.to_bits(),
            config.frequency_hz.to_bits(),
            config.damping.to_bits(),
            config.range.low.to_bits(),
            config.range.high.to_bits(),
            state.target.to_bits(),
        ],
        PreemWidget::FlipBoard { config, .. } => config
            .duration_secs
            .iter()
            .chain(config.stagger_secs.iter())
            .map(|v| v.to_bits())
            .collect(),
    }
}

/// The probe is real: an **unclamped** widget carrying a `NaN` is not equal to
/// itself, which is exactly the defect the clamp exists to close (a `NaN`
/// defeats the derived `PartialEq`, so neither end's render dedup ever
/// short-circuits). Without this, a clamp that silently did nothing could still
/// pass the equality tests below on a widget that never carried a `NaN`.
#[test]
fn an_unclamped_nan_widget_is_not_equal_to_itself() {
    let float_carrying = ["led-strip", "marquee", "scope", "gauge", "flip-board"];
    let mut seen = 0;
    for widget in widgets_with_every_float_poisoned(f32::NAN) {
        if float_carrying.contains(&widget.kind()) {
            seen += 1;
            assert_ne!(
                widget.clone(),
                widget,
                "{} carries no NaN — the poison probe missed a field",
                widget.kind()
            );
        }
    }
    assert_eq!(
        seen,
        float_carrying.len(),
        "a float-carrying kind went missing"
    );
}

/// Invariant (a): after clamping, every float of every widget kind is finite
/// and inside its bounds — even when the input carried a `NaN` or an infinity
/// in *every* float field it has.
#[test]
fn poisoned_floats_are_finite_and_in_range_after_clamping() {
    for poison in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        for widget in widgets_with_every_float_poisoned(poison) {
            let kind = widget.kind();
            let clamped = widget.clamped();
            assert_eq!(kind, clamped.kind(), "clamping changed the variant");
            assert_every_float_is_sane(&clamped);
        }
    }
}

/// Invariant (b): a clamped widget compares equal to itself, for every kind,
/// however poisoned the input was. This is the property both ends' render dedup
/// rests on — the shell's `applied == widget` gate and the SDK's
/// `view != last_view`.
#[test]
fn a_clamped_widget_compares_equal_to_itself() {
    for poison in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        for widget in widgets_with_every_float_poisoned(poison) {
            let kind = widget.kind();
            assert_eq!(
                widget.clone().clamped(),
                widget.clamped(),
                "{kind} does not compare equal to itself after clamping"
            );
        }
    }
}

/// Clamping is a **fixpoint**: a host that clamps a widget it already clamped
/// (or an SDK that clamps before the host does) must not see it move, or the
/// equality gates would fire on the second pass instead of the first.
#[test]
fn clamping_is_a_fixpoint_even_on_a_poisoned_widget() {
    for poison in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        for widget in widgets_with_every_float_poisoned(poison) {
            let once = widget.clamped();
            assert_eq!(
                once.clone().clamped(),
                once,
                "{} is not a clamp fixpoint",
                once.kind()
            );
        }
    }
}

/// The mapping table on `PreemWidget::clamp_in_place` is contract, so the four
/// tests below pin every row of it, one widget kind each.
/// (`speed_dots_per_sec` has its own test above,
/// `marquee_speed_is_clamped_including_non_finite`, and is the one row that
/// maps `±inf` to `0.0` rather than to the nearer bound.)
///
/// `LedStrip`, all three fields against the kit's *total* clamps
/// (`led_strip.rs:65` `lit_count`, `:80` `peak_led`, `:108` `PeakHold::new`):
/// level → rest/full/rest, the peak-hold rate → never-falls/cap/never-falls,
/// and an explicit peak stays **`Some`** — `Some(0.0)` is how the wire says
/// "no dot", which is what the kit draws for a `NaN` or a non-positive peak;
/// `None` would mean "use the shell-held decaying peak" instead, a different
/// render.
#[test]
fn non_finite_led_strip_readings_map_to_their_documented_replacements() {
    for (poison, want_level, want_rate, want_peak) in [
        (f32::NAN, 0.0_f32, 0.0_f32, 0.0_f32),
        (f32::INFINITY, 1.0, MAX_PEAK_HOLD_RATE, 1.0),
        (f32::NEG_INFINITY, 0.0, 0.0, 0.0),
    ] {
        let clamped = PreemWidget::LedStrip {
            config: LedStripConfig {
                peak_hold: Some(PeakHoldConfig { rate: poison }),
                ..LedStripConfig::default()
            },
            state: LedStripState {
                level: poison,
                peak: Some(poison),
            },
        }
        .clamped();
        let PreemWidget::LedStrip { config, state } = clamped else {
            panic!("variant changed")
        };
        assert_bits(state.level, want_level, "level");
        assert_bits(
            state
                .peak
                .expect("an explicit peak stays explicit — never None"),
            want_peak,
            "peak",
        );
        assert_bits(
            config.peak_hold.expect("peak_hold survives").rate,
            want_rate,
            "peak_hold.rate",
        );
    }
}

/// `Scope`: each sample independently. The kit's `sanitize`
/// (`scope.rs:386`) is a **guard**, not a bare clamp, so `NaN` *and* both
/// infinities read as `0.0` — the axis — while finite samples clamp to the
/// rails and in-range ones pass through untouched.
#[test]
fn non_finite_scope_samples_map_to_their_documented_replacements() {
    let clamped = PreemWidget::Scope {
        config: ScopeConfig::default(),
        state: ScopeState {
            samples: vec![f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 2.0, -2.0, 0.25],
        },
    }
    .clamped();
    let PreemWidget::Scope { state, .. } = clamped else {
        panic!("variant changed")
    };
    assert_eq!(state.samples.len(), 6, "the batch must not be re-sized");
    for (got, want) in state
        .samples
        .iter()
        .zip([0.0_f32, 0.0, 0.0, 1.0, -1.0, 0.25])
    {
        assert_bits(*got, want, "sample");
    }
}

/// `Gauge`: the three spring knobs sit behind the kit's `is_finite` guards
/// (`gauge.rs:598`, `:181`, `:195`), which keep the value already in the
/// builder for **every** non-finite input — the kit default, on the fresh
/// builder the shell constructs per config — so all three poisons resolve to
/// the same defaults rather than saturating. The target is the one row that
/// saturates, because its kept value is a *live needle's* (`gauge.rs:223`) and
/// no stateless clamp can see it.
#[test]
fn non_finite_gauge_floats_map_to_their_documented_replacements() {
    for (poison, want_sweep, want_freq, want_damping, want_target) in [
        (f32::NAN, 150.0_f32, 2.0_f32, 0.5_f32, 0.0_f32),
        (f32::INFINITY, 150.0, 2.0, 0.5, 1.0),
        (f32::NEG_INFINITY, 150.0, 2.0, 0.5, 0.0),
    ] {
        let clamped = PreemWidget::Gauge {
            config: GaugeConfig {
                sweep_deg: poison,
                frequency_hz: poison,
                damping: poison,
                range: GaugeRange {
                    low: poison,
                    high: poison,
                },
                ..GaugeConfig::default()
            },
            state: GaugeState { target: poison },
        }
        .clamped();
        let PreemWidget::Gauge { config, state } = clamped else {
            panic!("variant changed")
        };
        assert_bits(config.sweep_deg, want_sweep, "sweep_deg");
        assert_bits(config.frequency_hz, want_freq, "frequency_hz");
        assert_bits(config.damping, want_damping, "damping");
        // The poisoned range fell back to the default 0.0..=1.0 scale, and the
        // target landed at an end of *that*.
        assert_bits(config.range.low, 0.0, "range.low");
        assert_bits(config.range.high, 1.0, "range.high");
        assert_bits(state.target, want_target, "target");
    }

    // …and against a legal, non-default range the target's replacements are the
    // ends of that range, not of the default one.
    let range = GaugeRange {
        low: -20.0,
        high: 40.0,
    };
    for (poison, want_target) in [
        (f32::NAN, -20.0_f32),
        (f32::INFINITY, 40.0),
        (f32::NEG_INFINITY, -20.0),
    ] {
        let clamped = PreemWidget::Gauge {
            config: GaugeConfig {
                range,
                ..GaugeConfig::default()
            },
            state: GaugeState { target: poison },
        }
        .clamped();
        let PreemWidget::Gauge { state, .. } = clamped else {
            panic!("variant changed")
        };
        assert_bits(state.target, want_target, "target in a -20..=40 range");
    }
}

/// `FlipBoard`: both optional timings drop to `None` on anything non-finite —
/// `None` is how this field spells "the mechanism's own default" — and finite
/// values clamp to the kit's bounds.
#[test]
fn non_finite_flip_timings_drop_to_none() {
    for poison in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let clamped = PreemWidget::FlipBoard {
            config: FlipBoardConfig {
                duration_secs: Some(poison),
                stagger_secs: Some(poison),
                ..FlipBoardConfig::default()
            },
            state: FlipBoardState::default(),
        }
        .clamped();
        let PreemWidget::FlipBoard { config, .. } = clamped else {
            panic!("variant changed")
        };
        assert!(
            config.duration_secs.is_none() && config.stagger_secs.is_none(),
            "non-finite flip timings must drop to None, got {:?}/{:?}",
            config.duration_secs,
            config.stagger_secs
        );
    }
    let clamped = PreemWidget::FlipBoard {
        config: FlipBoardConfig {
            duration_secs: Some(100.0),
            stagger_secs: Some(-1.0),
            ..FlipBoardConfig::default()
        },
        state: FlipBoardState::default(),
    }
    .clamped();
    let PreemWidget::FlipBoard { config, .. } = clamped else {
        panic!("variant changed")
    };
    assert_bits(
        config.duration_secs.expect("kept"),
        MAX_FLIP_DURATION_SECS,
        "duration_secs",
    );
    assert_bits(config.stagger_secs.expect("kept"), 0.0, "stagger_secs");
}

/// The structural half of the parity rule: for **every** field the mapping
/// table on [`PreemWidget::clamp_in_place`] marks "guarded" — the scope's
/// samples, the gauge's three spring knobs, `GaugeRange`'s two ends, and
/// `FlipBoardConfig`'s two optional timings — the three non-finite inputs
/// must map to the **same** value, because the kit cannot tell them apart (a
/// guard is stateful keep-previous, which a stateless clamp reproduces with
/// one constant for all three poisons; see the table for what that constant
/// is per field). A future edit that "helpfully" saturates the infinities on
/// one of these fields breaks raster/state parity, and breaks here.
///
/// [`GaugeState::target`] is the documented exception — the one guarded field
/// the table says diverges (it draws `range.low` for `NaN`/`-inf` and
/// `range.high` for `+inf`, rather than one constant for all three) — and is
/// deliberately **not** enumerated here; see
/// `non_finite_gauge_floats_map_to_their_documented_replacements` for that
/// row's own coverage.
#[test]
#[allow(clippy::too_many_lines)] // four fields' worth of poison enumeration, not complexity
fn kit_guarded_fields_treat_every_non_finite_input_alike() {
    let poisons = [f32::NAN, f32::INFINITY, f32::NEG_INFINITY];

    let scoped: Vec<Vec<u32>> = poisons
        .iter()
        .map(|poison| {
            let clamped = PreemWidget::Scope {
                config: ScopeConfig::default(),
                state: ScopeState {
                    samples: vec![*poison],
                },
            }
            .clamped();
            float_bits(&clamped)
        })
        .collect();
    assert_eq!(scoped[0], scoped[1], "scope: NaN and +inf must agree");
    assert_eq!(scoped[0], scoped[2], "scope: NaN and -inf must agree");

    // The gauge's *config* knobs only — the target is the documented
    // divergence, so compare the three knobs rather than the whole widget.
    let knobs: Vec<[u32; 3]> = poisons
        .iter()
        .map(|poison| {
            let clamped = PreemWidget::Gauge {
                config: GaugeConfig {
                    sweep_deg: *poison,
                    frequency_hz: *poison,
                    damping: *poison,
                    ..GaugeConfig::default()
                },
                state: GaugeState::default(),
            }
            .clamped();
            let PreemWidget::Gauge { config, .. } = clamped else {
                panic!("variant changed")
            };
            [
                config.sweep_deg.to_bits(),
                config.frequency_hz.to_bits(),
                config.damping.to_bits(),
            ]
        })
        .collect();
    assert_eq!(knobs[0], knobs[1], "gauge knobs: NaN and +inf must agree");
    assert_eq!(knobs[0], knobs[2], "gauge knobs: NaN and -inf must agree");
    // …and they agree on the kit's defaults, not on a bound.
    let default_knobs = [150.0_f32.to_bits(), 2.0_f32.to_bits(), 0.5_f32.to_bits()];
    assert_eq!(
        knobs[0], default_knobs,
        "gauge knobs must keep the defaults"
    );

    // `FlipBoardConfig::duration_secs` / `stagger_secs`: the table's row for
    // both says `None` for every one of the three poisons — the mechanism's
    // own default, which is what this vocabulary spells as `None`.
    let flip_timings: Vec<[Option<u32>; 2]> = poisons
        .iter()
        .map(|poison| {
            let clamped = PreemWidget::FlipBoard {
                config: FlipBoardConfig {
                    duration_secs: Some(*poison),
                    stagger_secs: Some(*poison),
                    ..FlipBoardConfig::default()
                },
                state: FlipBoardState::default(),
            }
            .clamped();
            let PreemWidget::FlipBoard { config, .. } = clamped else {
                panic!("variant changed")
            };
            [
                config.duration_secs.map(f32::to_bits),
                config.stagger_secs.map(f32::to_bits),
            ]
        })
        .collect();
    assert_eq!(
        flip_timings[0], flip_timings[1],
        "flip board timings: NaN and +inf must agree"
    );
    assert_eq!(
        flip_timings[0], flip_timings[2],
        "flip board timings: NaN and -inf must agree"
    );
    assert_eq!(
        flip_timings[0],
        [None, None],
        "flip board timings must drop to None, matching the mapping table"
    );

    // `GaugeRange::low` / `high`: the table says a non-finite end replaces the
    // *whole* range with `GaugeRange::default()` (`0.0..=1.0`) as a unit, for
    // all three poisons alike — driven through `low` here, with `high` held at
    // a value the default does not already share, so a pass-through low would
    // show up as a range that isn't `0.0..=1.0`.
    let ranges: Vec<[u32; 2]> = poisons
        .iter()
        .map(|poison| {
            let clamped = PreemWidget::Gauge {
                config: GaugeConfig {
                    range: GaugeRange {
                        low: *poison,
                        high: 40.0,
                    },
                    ..GaugeConfig::default()
                },
                state: GaugeState::default(),
            }
            .clamped();
            let PreemWidget::Gauge { config, .. } = clamped else {
                panic!("variant changed")
            };
            [config.range.low.to_bits(), config.range.high.to_bits()]
        })
        .collect();
    assert_eq!(ranges[0], ranges[1], "gauge range: NaN and +inf must agree");
    assert_eq!(ranges[0], ranges[2], "gauge range: NaN and -inf must agree");
    assert_eq!(
        ranges[0],
        [0.0_f32.to_bits(), 1.0_f32.to_bits()],
        "a non-finite range end must fall back to the default 0.0..=1.0 scale, as a unit"
    );
}

/// A degenerate [`GaugeRange`] — inverted, empty, non-finite, or with a span
/// that overflows to infinity — is replaced by the default `0.0..=1.0` scale
/// **as a unit**, because the needle physics divides by the span. A legal range
/// is left exactly alone.
#[test]
fn a_degenerate_gauge_range_falls_back_to_the_default_scale() {
    for range in [
        GaugeRange {
            low: 10.0,
            high: 5.0,
        },
        GaugeRange {
            low: 3.0,
            high: 3.0,
        },
        GaugeRange {
            low: f32::NAN,
            high: 1.0,
        },
        GaugeRange {
            low: 0.0,
            high: f32::INFINITY,
        },
        GaugeRange {
            low: -f32::MAX,
            high: f32::MAX,
        },
    ] {
        let clamped = PreemWidget::Gauge {
            config: GaugeConfig {
                range,
                ..GaugeConfig::default()
            },
            state: GaugeState { target: 0.5 },
        }
        .clamped();
        let PreemWidget::Gauge { config, state } = clamped else {
            panic!("variant changed")
        };
        assert_bits(config.range.low, 0.0, "range.low");
        assert_bits(config.range.high, 1.0, "range.high");
        assert_bits(state.target, 0.5, "an in-range target rides the fallback");
    }

    let legal = GaugeRange {
        low: -20.0,
        high: 40.0,
    };
    let clamped = PreemWidget::Gauge {
        config: GaugeConfig {
            range: legal,
            ..GaugeConfig::default()
        },
        state: GaugeState { target: 1000.0 },
    }
    .clamped();
    let PreemWidget::Gauge { config, state } = clamped else {
        panic!("variant changed")
    };
    assert_bits(config.range.low, -20.0, "a legal range.low is untouched");
    assert_bits(config.range.high, 40.0, "a legal range.high is untouched");
    assert_bits(state.target, 40.0, "an over-range target clamps to the end");
}

/// The sanitiser must not perturb valid input: every float of every widget in
/// [`all_widgets`] comes back with the **same bit pattern**. Stricter than the
/// `==` in `clamping_a_legal_widget_changes_nothing`, which would let a
/// `-0.0` → `0.0` rewrite through.
#[test]
fn clamping_a_legal_widget_is_bit_identical() {
    for widget in all_widgets() {
        let clamped = widget.clone().clamped();
        assert_eq!(
            float_bits(&clamped),
            float_bits(&widget),
            "{} had a float rewritten despite being in range",
            widget.kind()
        );
        assert_eq!(
            clamped,
            widget,
            "{} was mutated by clamped()",
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

/// A [`StyleRef`] as it was encoded **before** #885: the skin, an optional
/// semantic role, and no `ink` key in the map at all. The field list and the
/// `skip_serializing_if` are a deliberate copy of the shipped struct's, so this
/// is the *old* encoder rather than a paraphrase of it.
#[derive(Debug, serde::Serialize)]
struct PreInkStyleRef {
    style: StyleName,
    #[serde(skip_serializing_if = "Option::is_none")]
    accent: Option<AccentRole>,
}

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

#[derive(serde::Serialize)]
struct OnlyLow {
    low: f32,
}

#[derive(serde::Serialize)]
struct OnlyRange {
    range: OnlyLow,
}

#[derive(serde::Serialize)]
struct SparseGaugeBody {
    config: OnlyRange,
    state: Empty,
}

/// A hand-rolled `{"Gauge": {"config": {"range": {"low": …}}, "state": {}}}`.
#[derive(serde::Serialize)]
struct SparseGauge {
    #[serde(rename = "Gauge")]
    gauge: SparseGaugeBody,
}

#[derive(serde::Serialize)]
struct OnlyPeakHold {
    peak_hold: Empty,
}

#[derive(serde::Serialize)]
struct SparseStripBody {
    config: OnlyPeakHold,
    state: Empty,
}

/// A hand-rolled `{"LedStrip": {"config": {"peak_hold": {}}, "state": {}}}`.
#[derive(serde::Serialize)]
struct SparseStrip {
    #[serde(rename = "LedStrip")]
    led_strip: SparseStripBody,
}

/// The `Node` vocabulary as it stood **before** #882 — every variant this crate
/// shipped at `VOCAB = 1`, minus `Preem`. Only the variant *names* matter (the
/// encoding is externally name-tagged), so the bodies are `serde_json`-style
/// catch-alls rather than faithful field lists.
#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
enum LegacyNode {
    Box(serde::de::IgnoredAny),
    Row(serde::de::IgnoredAny),
    ListBox(serde::de::IgnoredAny),
    Label(serde::de::IgnoredAny),
    Text(serde::de::IgnoredAny),
    Icon(serde::de::IgnoredAny),
    Pixels(serde::de::IgnoredAny),
    Button(serde::de::IgnoredAny),
    Progress(serde::de::IgnoredAny),
    Slider(serde::de::IgnoredAny),
    Revealer(serde::de::IgnoredAny),
    Separator(serde::de::IgnoredAny),
    Spacer,
    Expander(serde::de::IgnoredAny),
    Entry(serde::de::IgnoredAny),
}

/// The `HostMsg` set as it stood **before** #882 — every push this crate sent at
/// `VOCAB = 1`, minus `Hello`.
#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
enum LegacyHostMsg {
    StateSnapshot(serde::de::IgnoredAny),
    Event(serde::de::IgnoredAny),
    EffectResult(serde::de::IgnoredAny),
    SlotVisibility(serde::de::IgnoredAny),
    Accent(serde::de::IgnoredAny),
    AudioSpectrum(serde::de::IgnoredAny),
    ConsentDecision(serde::de::IgnoredAny),
    CalendarUpcoming(serde::de::IgnoredAny),
    SessionLocked(serde::de::IgnoredAny),
    NowPlaying(serde::de::IgnoredAny),
    DatasourceQuery(serde::de::IgnoredAny),
    DatasourceResult(serde::de::IgnoredAny),
    Ping(serde::de::IgnoredAny),
    Shutdown,
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
