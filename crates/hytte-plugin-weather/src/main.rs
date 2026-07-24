//! `hytte-plugin-weather` — the trollshell sidebar weather card, out of process
//! (issue #290, stage 3 of the #291 migration umbrella).
//!
//! A faithful port of the in-shell weather card (`trollshell`'s
//! `widgets/weather.rs` backed by `hytte_services::weather`) onto the
//! `hytte-plugin` SDK: pure TEA — the model below, one I/O worker, and a view
//! that mirrors the native card's content field-for-field (location, condition
//! icon, current temp, today's high/low, feels-like, wind, humidity).
//!
//! # I/O (`fetch.rs`)
//!
//! All network work runs off the reducer in the worker task the plugin's
//! [`sources`](Plugin::sources) spawns: it geolocates once at startup (geoclue,
//! then the `TROLLSHELL_WEATHER_CITY` fallback — [`location`]), then fetches
//! open-meteo every [`fetch::POLL_INTERVAL`] and on demand. Each result
//! re-enters the reducer as [`Input::App`]. **Click the card to refresh now**:
//! the click dispatches a [`Cmd`](Plugin::Cmd) down the #280 command lane, which
//! the worker turns into an immediate fetch. Slot-visibility gating
//! ([`Input::SlotVisible`], #288) is deliberately unused: at a 15-minute cadence
//! there is nothing worth parking while the sidebar is closed. The manifest
//! therefore does **not** subscribe [`StateKey::SlotVisible`], so under the
//! opt-in push rule (#305) the host never sends the visibility frame at all —
//! the reducer still folds a stray `SlotVisible` to a no-op as a belt-and-braces
//! guard.
//!
//! # Location (`location.rs`)
//!
//! Geolocation is geoclue over D-Bus. A plugin cannot use the shell's
//! `hytte-bus` layer (it transitively links gtk4 and spawns onto the shell's
//! global runtime), so it opens its own GTK-free `zbus` connection directly —
//! see [`location`]'s module docs for the full rationale.
//!
//! # Styling
//!
//! The card reuses the shell's own `ts-weather*` CSS classes verbatim (the host
//! applies classes as-is and the shell's stylesheet is loaded process-wide), so
//! the plugin card renders pixel-identical to the native one with **no** new
//! CSS. The whole card is a flat `gtk::Button` (the refresh affordance).

mod fetch;
mod location;

use hytte_plugin::proto::{
    Capability, DatasourceError, DatasourceOutcome, Dir, Effect, EventKind, Manifest, Mount, Node,
    ProvidedDatasource,
};
use hytte_plugin::tokio_stream::wrappers::UnboundedReceiverStream;
use hytte_plugin::{CmdReceiver, CmdSender, Input, MsgStream, Plugin};
use tokio::sync::mpsc;

/// Stable plugin id — the host's region key and audit-log subject.
const PLUGIN_ID: &str = "weather";
/// Placement request within the mount region: a low `order` renders earlier
/// (higher). Weather now mounts in the **leading** region ([`Mount::SidebarLead`],
/// #301) where it's the sole card today, so this is harmless — it's kept so that
/// if a second lead-region plugin ever appears, weather still sorts to the top.
const ORDER: i32 = -10;

/// The whole card is a flat button; a click is the "refresh now" target.
const CARD_BTN: &str = "weather-card";
/// The `weather` datasource's one scope (#509): the current reading. The
/// provider↔requester JSON payload is a [`WeatherJson`] object.
const SCOPE_CURRENT: &str = "current";
// Ids on the dynamic labels so the host reconciler mutates text in place across
// renders rather than rebuilding the subtree.
const LOC_ID: &str = "weather-loc";
const ICON_ID: &str = "weather-icon";
const TEMP_ID: &str = "weather-temp";
const COND_ID: &str = "weather-cond";
const MINMAX_ID: &str = "weather-minmax";
const FEELS_ID: &str = "weather-feels";
const WIND_ID: &str = "weather-wind";
const HUMID_ID: &str = "weather-humid";

/// Shown while location resolution or the first fetch is still in flight.
const LOADING_TEXT: &str = "Loading weather…";
/// A transient network failure with no prior good data (mirrors the native
/// card's error copy).
const NETWORK_ERR: &str = "network error";
/// No location source at all — the actionable message the native path shows,
/// verbatim.
const NO_LOCATION_MSG: &str = "No location — enable GeoClue or set $TROLLSHELL_WEATHER_CITY";

/// A weather condition: the raw WMO code plus a display label and a freedesktop
/// symbolic icon name. Ported from `hytte_services::weather::Condition`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Condition {
    pub(crate) code: u8,
    pub(crate) label: &'static str,
    pub(crate) icon: &'static str,
}

/// Pure mapping of a WMO weather code to a [`Condition`]. Unmapped codes fall
/// through to a generic severe-alert glyph. Ported verbatim (with its tests)
/// from `hytte_services::weather::condition_for_code`.
pub(crate) fn condition_for_code(code: u8) -> Condition {
    let (label, icon) = match code {
        0 => ("Clear", "weather-clear-symbolic"),
        1..=3 => ("Partly cloudy", "weather-few-clouds-symbolic"),
        45 | 48 => ("Fog", "weather-fog-symbolic"),
        51 | 53 | 55 | 56 | 57 | 61 | 63 | 65 | 66 | 67 => ("Rain", "weather-showers-symbolic"),
        71 | 73 | 75 | 77 => ("Snow", "weather-snow-symbolic"),
        80..=82 => ("Showers", "weather-showers-scattered-symbolic"),
        85 | 86 => ("Snow showers", "weather-snow-symbolic"),
        95 | 96 | 99 => ("Thunderstorm", "weather-storm-symbolic"),
        _ => ("Unknown", "weather-severe-alert-symbolic"),
    };
    Condition { code, label, icon }
}

/// One resolved weather reading — the content the card renders. Mirrors the
/// native `WeatherSnapshot` minus `fetched_at` (the card shows no timestamp).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Snapshot {
    pub(crate) location: String,
    pub(crate) temp_c: f64,
    pub(crate) apparent_c: f64,
    pub(crate) temp_max_c: f64,
    pub(crate) temp_min_c: f64,
    pub(crate) humidity_pct: u8,
    pub(crate) wind_kmh: f64,
    pub(crate) condition: Condition,
}

/// A message from the worker back into the reducer (an [`Input::App`]).
#[derive(Debug)]
pub(crate) enum WeatherMsg {
    /// A successful fetch produced a fresh reading.
    Weather(Snapshot),
    /// A fetch attempt failed (network). Last-good data, if any, is kept.
    FetchError,
    /// No location source resolved (no geoclue fix, `TROLLSHELL_WEATHER_CITY`
    /// unset) — surface the actionable error.
    NoLocation,
}

/// A command from the reducer to the worker (the #280 lane): the single one
/// this plugin issues is "fetch now".
#[derive(Debug, Clone, Copy)]
pub(crate) enum WeatherCmd {
    RefreshNow,
}

/// What the card currently shows.
#[derive(Debug, PartialEq)]
enum View {
    /// No reading yet — a compact placeholder.
    Loading,
    /// Last-good reading.
    Resolved(Snapshot),
    /// An error line (network blip with no prior data, or no location).
    Error(String),
}

/// The plugin's whole state. Rebuilt on every (re)connect; the host stores
/// nothing.
struct Weather {
    state: View,
    /// The command lane to the worker (#280): a card click sends
    /// [`WeatherCmd::RefreshNow`] here.
    cmd_tx: CmdSender<WeatherCmd>,
}

impl Weather {
    /// Fold one worker message into the model.
    fn on_msg(&mut self, msg: WeatherMsg) {
        match msg {
            WeatherMsg::Weather(snap) => self.state = View::Resolved(snap),
            // Keep a good prior reading rather than flashing an error on a
            // transient blip — exactly the native card's stance.
            WeatherMsg::FetchError => {
                if !matches!(self.state, View::Resolved(_)) {
                    self.state = View::Error(NETWORK_ERR.to_owned());
                }
            }
            // Only surface "no location" when there's nothing good to show.
            WeatherMsg::NoLocation => {
                if !matches!(self.state, View::Resolved(_)) {
                    self.state = View::Error(NO_LOCATION_MSG.to_owned());
                }
            }
        }
    }

    /// Answer a `weather` datasource query (#509) from the current reading. Served
    /// from the model — Loading / an error surface as a provider-sourced failure.
    fn answer_query(&self, request_id: u64) -> Effect {
        let outcome = match &self.state {
            View::Resolved(snap) => {
                let payload = WeatherJson {
                    location: &snap.location,
                    temp_c: snap.temp_c,
                    apparent_c: snap.apparent_c,
                    temp_max_c: snap.temp_max_c,
                    temp_min_c: snap.temp_min_c,
                    humidity_pct: snap.humidity_pct,
                    wind_kmh: snap.wind_kmh,
                    condition_code: snap.condition.code,
                    condition_label: snap.condition.label,
                    condition_icon: snap.condition.icon,
                };
                match serde_json::to_string(&payload) {
                    Ok(p) => DatasourceOutcome::Ready(p),
                    Err(e) => DatasourceOutcome::Failed {
                        error: DatasourceError::Provider,
                        message: format!("serialize weather: {e}"),
                    },
                }
            }
            View::Loading => DatasourceOutcome::Failed {
                error: DatasourceError::Provider,
                message: "weather not loaded yet".to_owned(),
            },
            View::Error(msg) => DatasourceOutcome::Failed {
                error: DatasourceError::Provider,
                message: msg.clone(),
            },
        };
        Effect::DatasourceResult {
            request_id,
            outcome,
        }
    }
}

/// The `weather` datasource payload — the JSON contract a requester decodes (#509).
/// A flat object mirroring [`Snapshot`], with the condition split into its raw WMO
/// `code` plus the display `label`/`icon` so a requester needn't re-derive them.
#[derive(serde::Serialize)]
struct WeatherJson<'a> {
    location: &'a str,
    temp_c: f64,
    apparent_c: f64,
    temp_max_c: f64,
    temp_min_c: f64,
    humidity_pct: u8,
    wind_kmh: f64,
    condition_code: u8,
    condition_label: &'a str,
    condition_icon: &'a str,
}

impl Plugin for Weather {
    type Msg = WeatherMsg;
    /// Outbound: the plugin's own fetch trigger rides the command lane (#280),
    /// not `update`'s effect return (a refresh is plugin I/O, not a shell
    /// effect).
    type Cmd = WeatherCmd;

    fn manifest() -> Manifest {
        // `SidebarLead` (#301): the leading region, above the built-in cards —
        // where the native weather card sits — so the plugin card actually leads
        // the sidebar (the after-tasks `SidebarTop` region can't). No state
        // subscriptions: weather sources its own data and doesn't gate on
        // visibility (see the crate docs), so it opts into no host push — which
        // also keeps it clear of the #305 hazard. No capabilities either (it
        // renders + fetches; it asks nothing of the host). It DOES serve its
        // reading as the `weather` datasource (#509) — `DatasourceProvider` +
        // `provides` — so the infobroker can source weather through this running
        // plugin. Scope `current` = the latest reading the card holds.
        let mut m = Manifest::new(PLUGIN_ID, Mount::SidebarLead).with_order(ORDER);
        m.capabilities = vec![Capability::DatasourceProvider];
        m.provides = vec![ProvidedDatasource::new(
            PLUGIN_ID,
            vec![SCOPE_CURRENT.to_owned()],
        )];
        m
    }

    fn init(cmds: CmdSender<Self::Cmd>) -> Self {
        Self {
            state: View::Loading,
            cmd_tx: cmds,
        }
    }

    fn sources(cmds: CmdReceiver<Self::Cmd>) -> Option<MsgStream<Self::Msg>> {
        // The worker owns both directions of the plugin's I/O: it drains the
        // command lane (`cmds`) and re-emits fetch results as the app messages
        // this stream carries.
        let (msg_tx, msg_rx) = mpsc::unbounded_channel();
        tokio::spawn(fetch::run(cmds, msg_tx));
        Some(Box::pin(UnboundedReceiverStream::new(msg_rx)))
    }

    fn update(&mut self, input: Input<Self::Msg>) -> Vec<Effect> {
        match input {
            Input::App(msg) => self.on_msg(msg),
            // Fire-and-forget: a card click is plugin I/O (a refresh), not a
            // shell effect, so the view and the effect batch are both unchanged.
            // `send` errs only mid-teardown — safe to drop.
            Input::Event { node, kind } if node == CARD_BTN && matches!(kind, EventKind::Click) => {
                let _ = self.cmd_tx.send(WeatherCmd::RefreshNow);
            }
            // #509: answer a `weather` datasource query from the current reading
            // (the card keeps it live off its worker, so there's nothing to fetch).
            // The single scope carries no params, so both are ignored.
            Input::DatasourceQuery { request_id, .. } => {
                return vec![self.answer_query(request_id)];
            }
            // Weather subscribes to no state and issues no effects, so `Snapshot`
            // and `EffectResult` (and a foreign/scroll `Event`) are no-ops.
            // `SlotVisible` (#288) is deliberately unused too — see the crate
            // docs — and the bare wildcard (rather than an exhaustive arm list)
            // keeps any future additive `Input` variant a no-op as well.
            _ => {}
        }
        Vec::new()
    }

    fn view(&self) -> hytte_plugin::View {
        let content = match &self.state {
            View::Loading => loading_content(),
            View::Resolved(snap) => resolved_content(snap),
            View::Error(msg) => error_content(msg),
        };
        // The whole card is a flat button — the refresh target. `flat` (an
        // Adwaita built-in) strips the button chrome; `ts-weather` gives it the
        // card background / padding / radius, identical to the native card.
        Node::Button {
            id: CARD_BTN.to_owned(),
            classes: vec!["flat".to_owned(), "ts-weather".to_owned()],
            child: Box::new(content),
        }
        .into()
    }
}

/// A compact "loading" placeholder — never a broken-looking card.
fn loading_content() -> Node {
    vbox(
        0,
        Vec::new(),
        vec![Node::Label {
            id: None,
            text: LOADING_TEXT.to_owned(),
            classes: vec!["ts-weather-condition".to_owned()],
        }],
    )
}

/// An error line: a warning glyph (`.ts-weather-error image` tints it) beside a
/// wrapping message, so a long line (the no-location hint) never blows the card
/// wide.
fn error_content(msg: &str) -> Node {
    Node::Box {
        id: None,
        dir: Dir::Horizontal,
        spacing: 8,
        scroll: false,
        classes: vec!["ts-weather-error".to_owned()],
        children: vec![
            Node::Icon {
                id: None,
                name: "dialog-warning-symbolic".to_owned(),
                classes: Vec::new(),
            },
            Node::Text {
                id: None,
                text: msg.to_owned(),
                max_width_chars: None,
                ellipsize: false,
                classes: Vec::new(),
            },
        ],
    }
}

/// The resolved card — mirrors `widgets/weather.rs`'s two-column layout and
/// classes field-for-field.
fn resolved_content(s: &Snapshot) -> Node {
    let headline = Node::Box {
        id: None,
        dir: Dir::Horizontal,
        spacing: 8,
        scroll: false,
        classes: vec!["ts-weather-headline".to_owned()],
        children: vec![
            Node::Icon {
                id: Some(ICON_ID.to_owned()),
                name: s.condition.icon.to_owned(),
                classes: vec!["ts-weather-icon".to_owned()],
            },
            label(TEMP_ID, format!("{:.0}°", s.temp_c), "ts-weather-temp"),
        ],
    };
    let left = vbox(
        0,
        Vec::new(),
        vec![
            headline,
            text_line(
                COND_ID,
                s.condition.label.to_owned(),
                "ts-weather-condition",
            ),
            label(
                MINMAX_ID,
                format!("↑ {:.0}°   ↓ {:.0}°", s.temp_max_c, s.temp_min_c),
                "ts-weather-minmax",
            ),
        ],
    );
    let details = vbox(
        2,
        vec!["ts-weather-details".to_owned()],
        vec![
            detail_row("Feels like", FEELS_ID, format!("{:.0}°", s.apparent_c)),
            detail_row("Wind", WIND_ID, format!("{:.0} km/h", s.wind_kmh)),
            detail_row("Humidity", HUMID_ID, format!("{}%", s.humidity_pct)),
        ],
    );
    let columns = Node::Box {
        id: None,
        dir: Dir::Horizontal,
        spacing: 16,
        scroll: false,
        classes: vec!["ts-weather-columns".to_owned()],
        children: vec![left, details],
    };
    vbox(
        0,
        Vec::new(),
        vec![
            text_line(LOC_ID, s.location.to_uppercase(), "ts-weather-location"),
            columns,
        ],
    )
}

/// A "name … value" detail row: `label + Spacer + value`, so the [`Node::Spacer`]
/// eats the row's horizontal slack and the value **right-pins** — the #299
/// justification primitive, mirroring the native card's `name.hexpand` +
/// `value.halign(End)` two-column look (this is the #295 "no hexpand" gap, now
/// closed).
fn detail_row(name: &str, value_id: &str, value: String) -> Node {
    Node::Box {
        id: None,
        dir: Dir::Horizontal,
        spacing: 8,
        scroll: false,
        classes: vec!["ts-weather-detail".to_owned()],
        children: vec![
            Node::Label {
                id: None,
                text: name.to_owned(),
                classes: vec!["ts-weather-detail-label".to_owned()],
            },
            Node::Spacer,
            label(value_id, value, "ts-weather-detail-value"),
        ],
    }
}

/// A vertical `Box` helper (the card is all vertical stacks).
fn vbox(spacing: i32, classes: Vec<String>, children: Vec<Node>) -> Node {
    Node::Box {
        id: None,
        dir: Dir::Vertical,
        spacing,
        scroll: false,
        classes,
        children,
    }
}

/// An id'd, single-class `Label`.
fn label(id: &str, text: String, class: &str) -> Node {
    Node::Label {
        id: Some(id.to_owned()),
        text,
        classes: vec![class.to_owned()],
    }
}

/// An id'd, single-class **single-line** `Text` that truncates with a trailing
/// ellipsis instead of wrapping (`ellipsize: true`, #299). Used for the location
/// and condition strings: the native card keeps them on one line (plain
/// non-wrapping labels), and ellipsizing keeps a long location name from blowing
/// the card wide rather than clipping it. (Values/labels in detail rows stay
/// plain `Label`s — they're short and fixed.)
fn text_line(id: &str, text: String, class: &str) -> Node {
    Node::Text {
        id: Some(id.to_owned()),
        text,
        max_width_chars: None,
        ellipsize: true,
        classes: vec![class.to_owned()],
    }
}

fn main() {
    hytte_plugin::run::<Weather>();
}

#[cfg(test)]
mod tests {
    use super::{
        CARD_BTN, COND_ID, Condition, ICON_ID, LOADING_TEXT, LOC_ID, NETWORK_ERR, NO_LOCATION_MSG,
        Snapshot, TEMP_ID, View, Weather, WeatherCmd, WeatherMsg, condition_for_code,
    };
    use hytte_plugin::proto::{EventKind, Manifest, Mount, Node};
    use hytte_plugin::{CmdReceiver, Input, Plugin};

    /// A fresh model plus a probe on its command lane (the test plays the role
    /// the runtime normally does, keeping the receiver to assert dispatches).
    fn model() -> (Weather, CmdReceiver<WeatherCmd>) {
        let (tx, rx) = hytte_plugin::cmd_channel();
        (Weather::init(tx), rx)
    }

    fn sample() -> Snapshot {
        Snapshot {
            location: "Oberschöneweide".to_owned(),
            temp_c: 18.4,
            apparent_c: 16.1,
            temp_max_c: 22.0,
            temp_min_c: 14.0,
            humidity_pct: 64,
            wind_kmh: 12.3,
            condition: condition_for_code(3),
        }
    }

    /// Depth-first search for the text of a `Label`/`Text` node with `id`.
    fn find_text(node: &Node, id: &str) -> Option<String> {
        match node {
            Node::Label {
                id: Some(nid),
                text,
                ..
            }
            | Node::Text {
                id: Some(nid),
                text,
                ..
            } if nid == id => Some(text.clone()),
            Node::Box { children, .. } => children.iter().find_map(|c| find_text(c, id)),
            Node::Button { child, .. } => find_text(child, id),
            _ => None,
        }
    }

    /// Depth-first search for the `ellipsize` flag of a `Text` node with `id`.
    fn find_text_ellipsize(node: &Node, id: &str) -> Option<bool> {
        match node {
            Node::Text {
                id: Some(nid),
                ellipsize,
                ..
            } if nid == id => Some(*ellipsize),
            Node::Box { children, .. } => children.iter().find_map(|c| find_text_ellipsize(c, id)),
            Node::Button { child, .. } => find_text_ellipsize(child, id),
            _ => None,
        }
    }

    /// Depth-first search for the first `Box` carrying `class` (returns its
    /// children — the row shape we want to pin).
    fn find_box_children<'a>(node: &'a Node, class: &str) -> Option<&'a [Node]> {
        match node {
            Node::Box {
                classes, children, ..
            } => {
                if classes.iter().any(|c| c == class) {
                    Some(children)
                } else {
                    children.iter().find_map(|c| find_box_children(c, class))
                }
            }
            Node::Button { child, .. } => find_box_children(child, class),
            _ => None,
        }
    }

    /// Depth-first search for the icon name of an `Icon` node with `id`.
    fn find_icon(node: &Node, id: &str) -> Option<String> {
        match node {
            Node::Icon {
                id: Some(nid),
                name,
                ..
            } if nid == id => Some(name.clone()),
            Node::Box { children, .. } => children.iter().find_map(|c| find_icon(c, id)),
            Node::Button { child, .. } => find_icon(child, id),
            _ => None,
        }
    }

    /// True if any node in the tree carries `class`.
    fn has_class(node: &Node, class: &str) -> bool {
        let hit = |classes: &[String]| classes.iter().any(|c| c == class);
        match node {
            Node::Box {
                classes, children, ..
            } => hit(classes) || children.iter().any(|c| has_class(c, class)),
            Node::Button { classes, child, .. } => hit(classes) || has_class(child, class),
            Node::Label { classes, .. }
            | Node::Text { classes, .. }
            | Node::Icon { classes, .. } => hit(classes),
            _ => false,
        }
    }

    // ── Condition mapping (ported from hytte_services::weather) ──────────────

    #[test]
    fn condition_known_codes() {
        assert_eq!(condition_for_code(0).label, "Clear");
        assert_eq!(condition_for_code(0).icon, "weather-clear-symbolic");
        for c in [1, 2, 3] {
            assert_eq!(condition_for_code(c).label, "Partly cloudy");
        }
        for c in [45, 48] {
            assert_eq!(condition_for_code(c).icon, "weather-fog-symbolic");
        }
        assert_eq!(condition_for_code(61).label, "Rain");
        assert_eq!(condition_for_code(71).label, "Snow");
        assert_eq!(condition_for_code(80).label, "Showers");
        assert_eq!(condition_for_code(95).label, "Thunderstorm");
    }

    #[test]
    fn condition_unknown_code_is_severe_alert() {
        let c: Condition = condition_for_code(200);
        assert_eq!(c.label, "Unknown");
        assert_eq!(c.icon, "weather-severe-alert-symbolic");
        assert_eq!(c.code, 200);
    }

    // ── Manifest ─────────────────────────────────────────────────────────────

    #[test]
    fn manifest_leads_the_sidebar_and_subscribes_nothing() {
        let m: Manifest = Weather::manifest();
        assert_eq!(m.id, "weather");
        assert_eq!(
            m.mount,
            Mount::SidebarLead,
            "the leading region — above the built-in cards (#301)"
        );
        assert_eq!(m.order, Some(-10), "sole lead-region card; harmless order");
        assert!(
            m.subscribes.is_empty(),
            "weather sources its own data and opts into no host push — no SlotVisible (#305)"
        );
        // #509: weather serves its reading as the `weather` datasource, so it now
        // declares `DatasourceProvider` + a `provides` entry (nothing else).
        assert_eq!(
            m.capabilities,
            vec![hytte_plugin::proto::Capability::DatasourceProvider],
            "the only capability is the datasource-provider one (#509)"
        );
        assert_eq!(m.provides.len(), 1, "provides the `weather` datasource");
        assert_eq!(m.provides[0].id, "weather");
        assert!(m.provides[0].serves_scope("current"));
        m.check_proto()
            .expect("stamped with the current proto version");
    }

    // ── View states ──────────────────────────────────────────────────────────

    #[test]
    fn seed_view_is_a_compact_loading_placeholder() {
        let (m, _rx) = model();
        let tree = m.view().tree;
        let Node::Button { id, child, .. } = &tree else {
            panic!("the card root is the refresh button");
        };
        assert_eq!(id, CARD_BTN);
        // Not the full card: just the placeholder line, and no temp label.
        assert!(matches!(&**child, Node::Box { .. }));
        assert!(find_text(&tree, TEMP_ID).is_none(), "no reading yet");
        assert!(has_class(&tree, "ts-weather"), "still styled as a card");
        let Node::Box { children, .. } = &**child else {
            unreachable!()
        };
        assert!(
            matches!(&children[0], Node::Label { text, .. } if text == LOADING_TEXT),
            "the placeholder shows the loading line"
        );
    }

    #[test]
    fn resolved_view_renders_every_native_field() {
        let (mut m, _rx) = model();
        let _ = m.update(Input::App(WeatherMsg::Weather(sample())));
        assert!(matches!(m.state, View::Resolved(_)));
        let tree = m.view().tree;

        assert_eq!(find_text(&tree, LOC_ID).as_deref(), Some("OBERSCHÖNEWEIDE"));
        assert_eq!(
            find_icon(&tree, ICON_ID).as_deref(),
            Some("weather-few-clouds-symbolic")
        );
        assert_eq!(find_text(&tree, TEMP_ID).as_deref(), Some("18°"));
        assert_eq!(find_text(&tree, COND_ID).as_deref(), Some("Partly cloudy"));
        assert_eq!(
            find_text(&tree, "weather-minmax").as_deref(),
            Some("↑ 22°   ↓ 14°")
        );
        assert_eq!(find_text(&tree, "weather-feels").as_deref(), Some("16°"));
        assert_eq!(find_text(&tree, "weather-wind").as_deref(), Some("12 km/h"));
        assert_eq!(find_text(&tree, "weather-humid").as_deref(), Some("64%"));
    }

    /// The #301 layout adoption: detail rows are `label + Spacer + value` (so the
    /// value right-pins, #299), and the single-line strings (location, condition)
    /// ride ellipsizing `Text` so a long value truncates instead of blowing the
    /// card wide.
    #[test]
    fn resolved_view_right_pins_details_and_ellipsizes_single_lines() {
        let (mut m, _rx) = model();
        let _ = m.update(Input::App(WeatherMsg::Weather(sample())));
        let tree = m.view().tree;

        // Location + condition stay single-line via ellipsizing Text.
        assert_eq!(
            find_text_ellipsize(&tree, LOC_ID),
            Some(true),
            "location ellipsizes"
        );
        assert_eq!(
            find_text_ellipsize(&tree, COND_ID),
            Some(true),
            "condition ellipsizes"
        );

        // Each detail row is `label + Spacer + value`, so the value right-pins.
        let row = find_box_children(&tree, "ts-weather-detail").expect("a detail row");
        assert!(
            matches!(
                row,
                [
                    Node::Label { .. },
                    Node::Spacer,
                    Node::Label { id: Some(_), .. }
                ]
            ),
            "detail row is label + Spacer + value, got {row:?}",
        );
    }

    #[test]
    fn transient_fetch_error_keeps_last_good_data() {
        let (mut m, _rx) = model();
        let _ = m.update(Input::App(WeatherMsg::Weather(sample())));
        let _ = m.update(Input::App(WeatherMsg::FetchError));
        assert!(
            matches!(m.state, View::Resolved(_)),
            "a network blip must not clobber a good card"
        );
        assert_eq!(find_text(&m.view().tree, TEMP_ID).as_deref(), Some("18°"));
    }

    #[test]
    fn first_fetch_error_shows_network_error() {
        let (mut m, _rx) = model();
        let _ = m.update(Input::App(WeatherMsg::FetchError));
        assert_eq!(m.state, View::Error(NETWORK_ERR.to_owned()));
        assert!(has_class(&m.view().tree, "ts-weather-error"));
    }

    #[test]
    fn no_location_shows_the_actionable_message() {
        let (mut m, _rx) = model();
        let _ = m.update(Input::App(WeatherMsg::NoLocation));
        assert_eq!(m.state, View::Error(NO_LOCATION_MSG.to_owned()));
        // The message rides a wrapping Text node so a long hint can't blow the
        // card wide.
        let tree = m.view().tree;
        assert!(tree_has_text(&tree, NO_LOCATION_MSG));
    }

    #[test]
    fn no_location_does_not_clobber_a_resolved_card() {
        let (mut m, _rx) = model();
        let _ = m.update(Input::App(WeatherMsg::Weather(sample())));
        let _ = m.update(Input::App(WeatherMsg::NoLocation));
        assert!(matches!(m.state, View::Resolved(_)));
    }

    // ── Click → command lane ─────────────────────────────────────────────────

    #[test]
    fn clicking_the_card_requests_a_refresh() {
        let (mut m, mut rx) = model();
        let fx = m.update(Input::Event {
            node: CARD_BTN.to_owned(),
            kind: EventKind::Click,
        });
        assert!(fx.is_empty(), "a refresh is plugin I/O, not a shell effect");
        assert!(
            matches!(rx.try_recv(), Ok(WeatherCmd::RefreshNow)),
            "the click dispatched a refresh down the lane"
        );
    }

    #[test]
    fn clicking_a_foreign_node_requests_nothing() {
        let (mut m, mut rx) = model();
        let fx = m.update(Input::Event {
            node: "not-ours".to_owned(),
            kind: EventKind::Click,
        });
        assert!(fx.is_empty());
        assert!(rx.try_recv().is_err(), "foreign clicks are ignored");
    }

    #[test]
    fn ignored_inputs_are_no_ops() {
        // Snapshot / EffectResult / SlotVisible (and any future additive Input
        // variant) fold to nothing — no panic, no effect, no state change.
        // Visibility gating is deliberately unused at this cadence (crate docs).
        let (mut m, _rx) = model();
        let before = m.view();
        let fx = m.update(Input::Snapshot(
            hytte_plugin::proto::StateSnapshot::default(),
        ));
        assert!(fx.is_empty());
        let fx = m.update(Input::EffectResult {
            id: 1,
            outcome: hytte_plugin::proto::EffectOutcome {
                ok: true,
                output: None,
            },
        });
        assert!(fx.is_empty());
        let fx = m.update(Input::SlotVisible(false));
        assert!(fx.is_empty());
        assert_eq!(m.view(), before, "ignored inputs leave the card unchanged");
    }

    // ── small test helpers ───────────────────────────────────────────────────

    fn tree_has_text(node: &Node, needle: &str) -> bool {
        match node {
            Node::Label { text, .. } | Node::Text { text, .. } => text == needle,
            Node::Box { children, .. } => children.iter().any(|c| tree_has_text(c, needle)),
            Node::Button { child, .. } => tree_has_text(child, needle),
            _ => false,
        }
    }
}
