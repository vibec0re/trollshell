//! `hytte-plugin-departures` — the S-Bahn departures board, out-of-process 🚈
//! (issue #289, migration stage 2 of the #274-approved plugin migration).
//!
//! The **first** built-in widget to move out of the shell: a pure list UI over
//! a fully self-contained fetch loop, with no cross-widget data coupling. It
//! ports `hytte_services::departures` + `trollshell::widgets::departures` onto
//! the [`hytte_plugin`] TEA runtime — the model below, a visibility-gated
//! poller in [`feed`], and a `ListBox` of rows in [`Board::view`].
//!
//! # The visibility-gated poller — the pattern this plugin exists to prove
//!
//! The native board re-polls **only while the sidebar is open**
//! (`overlays/sidebar.rs`'s `REFRESH_WHILE_OPEN` = 30 s, plus an immediate
//! poll on the open edge). An out-of-process plugin can't see the shell's
//! open-state directly, so #288/PR #294 added the missing signal:
//! [`Input::SlotVisible`] arrives whenever the mount surface shows/hides
//! (and once, seeded, at register).
//!
//! Bridging that host push to the plugin's own I/O task is the reference
//! pattern for every future gated plugin — **`update` → command lane →
//! I/O task**:
//!
//! 1. [`update`](Board::update) folds each [`Input::SlotVisible(visible)`] and
//!    forwards it **down the command lane** as [`Cmd::SetVisible`] (the
//!    sanctioned outbound path, #280 — `update` is sync and can't do I/O).
//! 2. The [`feed::poll_task`] the [`sources`](Board::sources) spawn owns the
//!    fetch interval and drains that lane. While hidden it **parks** — no
//!    ticks, no HTTP. On a hidden→visible edge it does an **immediate refresh**
//!    (mirroring the native open-edge poll) and then re-polls every
//!    [`feed::REFRESH_WHILE_OPEN`] (30 s) until hidden again.
//!
//! The expensive work (the HTTP fetch) is what parks; per-second re-renders for
//! the live "in N min" / leave-by labels still ride the `Clock` subscription
//! exactly as the native widget's clock tick did (cheap, and needed for the
//! departed-row prune).
//!
//! # Station configuration
//!
//! Ported from the native board's `places.toml` mechanism
//! (`hytte-services::places`): the station + line/direction filter + walk
//! budget come from the **first `[[place]]`** of
//! `~/.config/trollshell/places.toml`, re-read on every fetch so a saved edit
//! is picked up live while the board is open. Without D-Bus this plugin can't
//! run the native Wi-Fi-fingerprint / `GeoClue` place *resolution*, so it always
//! shows that first (home) place — the same provisional-home the native
//! resolver falls back to before its first sensor fix. See [`feed`].
//!
//! # Model / view
//!
//! Rows carry a HAFAS `trip_id` (stable across refreshes) so #236's
//! arm-a-train leave-by nudge can fold in on top later. The transition machine
//! ([`next_state`]) and the leave-by label math ([`lead_label`]) are ported
//! verbatim from the native code and unit-tested here with the same edge cases.

mod feed;

use feed::Row;
use hytte_plugin::proto::{Capability, Effect, EventKind, Manifest, Mount, Node, StateKey};
use hytte_plugin::tokio_stream::wrappers::UnboundedReceiverStream;
use hytte_plugin::{CmdReceiver, CmdSender, Input, MsgStream, Plugin, View};
use tokio::sync::mpsc;

/// Stable plugin id — the host's mount-slot ownership key and audit-log subject.
const PLUGIN_ID: &str = "departures";
/// The root `ListBox` node id.
const ROOT_ID: &str = "departures-root";

/// How long after a train's actual departure we keep its row before hiding it.
/// Native `DEPARTED_GRACE` (30 s): absorbs clock skew and lets "now" linger a
/// beat rather than vanishing the instant the scheduled second ticks past.
const DEPARTED_GRACE_SECS: i64 = 30;
/// After this much elapses since the last good fetch, a continuing error drops
/// `Stale` → `Err`. Native `STALE_DROP_AFTER` (30 min).
const STALE_DROP_AFTER_SECS: i64 = 30 * 60;
/// Ellipsis/wrap cap on the destination cell. Native `set_max_width_chars(22)`.
const DIRECTION_MAX_CHARS: i32 = 22;

/// Arm-a-train leave-by nudge thresholds (#236), in **seconds of slack**: raise
/// the OSD once as the armed train's slack falls through 3 minutes, once as it
/// hits now/0. Diffed prev→curr each clock tick so each fires exactly once
/// (latching), mirroring the shell OSD's battery-threshold detection.
const LEAVE_SOON_SECS: i64 = 3 * 60;
const LEAVE_NOW_SECS: i64 = 0;

/// The symbolic icon the leave-by nudge asks the shell to show (the shell's
/// own default for the kind — sent explicitly so the intent is on the wire).
const LEAVE_BY_ICON: &str = "appointment-soon-symbolic";

// ── Messages / commands ──────────────────────────────────────────────────────

/// A message from the plugin's own [`feed::poll_task`] I/O source.
#[derive(Debug)]
pub(crate) enum BoardMsg {
    /// One completed fetch (or its error) to fold into [`BoardState`].
    Fetched(Result<Vec<Row>, String>),
}

/// A command from [`update`](Board::update) to the [`feed::poll_task`] over the
/// per-session command lane (#280) — the outbound bridge for the visibility
/// gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cmd {
    /// The mount surface became visible (`true`) / hidden (`false`). The task
    /// parks its poller while hidden and does an immediate refresh on the
    /// hidden→visible edge.
    SetVisible(bool),
}

// ── State ────────────────────────────────────────────────────────────────────

/// The whole board surface — the wire-side mirror of the native
/// `DeparturesState`. `at_unix` timestamps the last good fetch (for the stale
/// drop) and `at_hhmm` is its display time (for the stale footer).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub(crate) enum BoardState {
    /// Initial value, before the first fetch returns.
    #[default]
    Loading,
    /// Most recent fetch succeeded.
    Ok {
        at_unix: i64,
        at_hhmm: String,
        items: Vec<Row>,
    },
    /// A previous fetch succeeded and a later one failed; keep showing the
    /// prior list with a "stale" hint, up to [`STALE_DROP_AFTER_SECS`].
    Stale {
        at_unix: i64,
        at_hhmm: String,
        items: Vec<Row>,
        err: String,
    },
    /// No usable data on hand and the latest fetch failed.
    Err { err: String },
}

/// Apply a fetch result to the current state, returning the next one. Pure
/// port of the native `next_state` transition table:
///
/// | previous                                          | result   | next                          |
/// |---------------------------------------------------|----------|-------------------------------|
/// | any                                               | `Ok`     | `Ok { at: now, items }`       |
/// | `Ok`/`Stale`, age `< STALE_DROP_AFTER`            | `Err(e)` | `Stale { .., err: e }`        |
/// | `Stale`, age `>= STALE_DROP_AFTER`                | `Err(e)` | `Err { err: e }`              |
/// | `Loading`/`Err`                                   | `Err(e)` | `Err { err: e }`              |
fn next_state(
    prev: BoardState,
    result: Result<Vec<Row>, String>,
    now_unix: i64,
    now_hhmm: &str,
) -> BoardState {
    match result {
        Ok(items) => BoardState::Ok {
            at_unix: now_unix,
            at_hhmm: now_hhmm.to_owned(),
            items,
        },
        Err(err) => match prev {
            BoardState::Ok {
                at_unix,
                at_hhmm,
                items,
            } => BoardState::Stale {
                at_unix,
                at_hhmm,
                items,
                err,
            },
            BoardState::Stale {
                at_unix,
                at_hhmm,
                items,
                err: _,
            } => {
                if now_unix - at_unix >= STALE_DROP_AFTER_SECS {
                    BoardState::Err { err }
                } else {
                    BoardState::Stale {
                        at_unix,
                        at_hhmm,
                        items,
                        err,
                    }
                }
            }
            BoardState::Loading | BoardState::Err { .. } => BoardState::Err { err },
        },
    }
}

// ── Label math (ported verbatim from the native widget, on unix seconds) ──────

/// Human-readable "minutes from now". Negatives and anything within the next
/// 60 s render as `"now"`; above that, rounds to the nearest minute so
/// `"7 min"` covers `[6m31s, 7m30s]`. Native `relative_label`.
fn relative_label(now_unix: i64, actual_unix: i64) -> String {
    let seconds = actual_unix - now_unix;
    if seconds <= 60 {
        return "now".to_owned();
    }
    let minutes = (seconds + 30) / 60;
    format!("{minutes} min")
}

/// The relative token shown before "· HH:MM". With no walk budget it is the
/// plain departs-in [`relative_label`]. With a positive budget it is a
/// leave-by countdown — whole minutes until you must leave to still catch the
/// train — collapsing to `"now"` at zero. The returned bool is whether the
/// train is already unreachable (negative slack), which the caller renders
/// faded. Native `lead_label`.
fn lead_label(now_unix: i64, actual_unix: i64, walk_minutes: u32) -> (String, bool) {
    if walk_minutes == 0 {
        return (relative_label(now_unix, actual_unix), false);
    }
    // Seconds of slack: how long you can still wait before you must leave.
    let slack = slack_secs(now_unix, actual_unix, walk_minutes);
    let minutes = (slack + 30) / 60;
    let token = if minutes <= 0 {
        "now".to_owned()
    } else {
        format!("{minutes} min")
    };
    (token, slack < 0)
}

/// Seconds of leave-by slack: how long you can still wait before you must leave
/// to catch a train `walk_minutes` from the platform. Negative once you're
/// already too late. Factored out of [`lead_label`] so the arm-a-train tick
/// (#236) computes the crossing on the exact same math the label displays.
fn slack_secs(now_unix: i64, actual_unix: i64, walk_minutes: u32) -> i64 {
    (actual_unix - now_unix) - i64::from(walk_minutes) * 60
}

// ── Arm-a-train leave-by nudge (#236) ────────────────────────────────────────

/// Which leave-by threshold an armed train just crossed — the OSD title differs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LeaveByEvent {
    /// Slack fell through 3 minutes → "Leave soon".
    Soon,
    /// Slack hit now/0 → "Leave now".
    Now,
}

/// Edge-detect a leave-by threshold crossing: fire once as `curr_slack` falls
/// through 3 minutes, once as it falls through now/0. Mirrors the shell OSD's
/// `detect_battery_event` prev/curr latching — a steady state (slack already
/// below a threshold) re-fires nothing, and the more-urgent "now" wins a tick
/// that spans both. A `None` prev (the just-armed baseline) never fires; it only
/// seeds the diff.
fn detect_leave_by_crossing(prev_slack: Option<i64>, curr_slack: i64) -> Option<LeaveByEvent> {
    let prev = prev_slack?;
    if prev > LEAVE_NOW_SECS && curr_slack <= LEAVE_NOW_SECS {
        return Some(LeaveByEvent::Now);
    }
    if prev > LEAVE_SOON_SECS && curr_slack <= LEAVE_SOON_SECS {
        return Some(LeaveByEvent::Soon);
    }
    None
}

/// The bold OSD title for a crossing.
fn leave_by_title(event: LeaveByEvent) -> &'static str {
    match event {
        LeaveByEvent::Now => "Leave now",
        LeaveByEvent::Soon => "Leave soon",
    }
}

/// The OSD body line for an armed row: `"<line> · <direction> · <HH:MM>"`.
fn leave_by_body(r: &Row) -> String {
    format!("{} · {} · {}", r.line, r.direction, r.hhmm)
}

/// Build the shell effect for a leave-by crossing. The plugin computes the
/// display strings (title/body) and the shell just shows them (#236).
fn leave_by_effect(event: LeaveByEvent, body: &str) -> Effect {
    Effect::RaiseOsd {
        title: leave_by_title(event).to_owned(),
        body: body.to_owned(),
        icon: Some(LEAVE_BY_ICON.to_owned()),
    }
}

/// Whether a train counts as already gone — actual departure more than
/// [`DEPARTED_GRACE_SECS`] in the past. Native `departed`.
fn departed(now_unix: i64, actual_unix: i64) -> bool {
    now_unix - actual_unix > DEPARTED_GRACE_SECS
}

/// The delay badge after the time cell: `None` = render nothing; `Some("+5")`
/// = 5 minutes late. Only lateness is surfaced (early trains are silent).
/// Native `delay_string`.
fn delay_string(delay_minutes: i64) -> Option<String> {
    (delay_minutes > 0).then(|| format!("+{delay_minutes}"))
}

/// `HH:MM` sliced out of an RFC 3339 local timestamp
/// (`2026-07-11T15:49:00+02:00`), reusing the shell clock's own local
/// formatting rather than re-deriving a timezone. `--:--` for a short string.
fn hhmm(iso: &str) -> String {
    iso.get(11..16).unwrap_or("--:--").to_owned()
}

/// Sanitize a line name into a CSS-class-safe token so a stray non-alphanumeric
/// from the API (e.g. `"SEV S9"`) can't trip the host's `add_css_class`
/// assertion. Native `safe_line`.
fn safe_line(line: &str) -> String {
    line.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The error-row text. Fetch failures carry a lowercase `kind:` prefix from the
/// [`feed`] helpers (`http:` / `body:` / `decode:` / `join:`); those get the
/// native widget's "can't reach BVG" reachability context. A configuration
/// message (no such prefix) is shown as-is, so its actionable hint isn't buried.
fn error_text(err: &str) -> String {
    const NET_PREFIXES: [&str; 4] = ["http:", "body:", "decode:", "join:"];
    if NET_PREFIXES.iter().any(|p| err.starts_with(p)) {
        format!("can't reach BVG: {err}")
    } else {
        err.to_owned()
    }
}

// ── The plugin ───────────────────────────────────────────────────────────────

/// The board's whole state. Rebuilt on every (re)connect; re-derived from the
/// clock snapshot and the poll task's fetches (the design's per-session stance).
struct Board {
    /// Current departures state — the ported transition machine.
    state: BoardState,
    /// Latest clock, unix seconds — drives the leave-by labels + departed prune.
    now_unix: i64,
    /// Latest clock as its RFC 3339 string — sliced to stamp the last-good time
    /// (`at_hhmm`) on a successful fetch, tz-correct without re-deriving one.
    now_iso: String,
    /// The command lane to the [`feed::poll_task`]: `update` forwards each
    /// visibility change as a [`Cmd::SetVisible`] here.
    cmd_tx: CmdSender<Cmd>,
    /// The armed train's HAFAS `trip_id` (#236), or `None` when nothing is
    /// armed. Resolved against the **live** row set each clock tick (never a
    /// frozen arm-time copy), so a refresh that moves the trip's time is honored
    /// — the stale-snapshot guard.
    armed: Option<String>,
    /// The armed train's slack (seconds) at the previous clock tick, for the
    /// leave-by threshold latching (fire once per 3-min / now crossing). Seeded
    /// on arm, cleared on disarm; mirrors the OSD battery prev/curr diff.
    armed_prev_slack: Option<i64>,
}

impl Plugin for Board {
    type Msg = BoardMsg;
    /// Outbound: the visibility gate rides the command lane (#280), not
    /// `update`'s effect return (which is for *shell* effects — this board asks
    /// nothing of the shell).
    type Cmd = Cmd;

    fn manifest() -> Manifest {
        // SidebarBottom: where the native board anchored. Subscribes to Clock for
        // the per-second leave-by relabel + departed prune, and to SlotVisible
        // (#305) so the host actually pushes the visibility edges that park/resume
        // the poll task (`Input::SlotVisible` below) — the push is opt-in via the
        // manifest now, so a poller MUST subscribe to keep being gated. Requests
        // the `RaiseOsd` cap for arm-a-train's leave-by nudge (#236); no `order`
        // (sole card in the region today).
        let mut m = Manifest::new(PLUGIN_ID, Mount::SidebarBottom);
        m.subscribes = vec![StateKey::Clock, StateKey::SlotVisible];
        m.capabilities = vec![Capability::RaiseOsd];
        m
    }

    fn init(cmds: CmdSender<Self::Cmd>) -> Self {
        Self {
            state: BoardState::Loading,
            now_unix: 0,
            now_iso: String::new(),
            cmd_tx: cmds,
            armed: None,
            armed_prev_slack: None,
        }
    }

    fn sources(cmds: CmdReceiver<Self::Cmd>) -> Option<MsgStream<Self::Msg>> {
        // The poll task owns both directions of the board's own I/O: it drains
        // the command lane (visibility) and re-emits each fetch as a
        // `BoardMsg::Fetched` on this stream.
        let (msg_tx, msg_rx) = mpsc::unbounded_channel();
        tokio::spawn(feed::poll_task(cmds, msg_tx));
        Some(Box::pin(UnboundedReceiverStream::new(msg_rx)))
    }

    fn update(&mut self, input: Input<Self::Msg>) -> Vec<Effect> {
        match input {
            // The clock: drives relative/leave-by labels and the departed prune.
            // `clock` is optional on the wire (a startup snapshot may precede the
            // host's clock pump), so tolerate `None`. On each tick we also
            // re-evaluate the armed train — the one place a leave-by OSD nudge is
            // raised (#236).
            Input::Snapshot(snapshot) => {
                if let Some(clock) = snapshot.clock {
                    self.now_unix = clock.unix;
                    self.now_iso = clock.iso;
                }
                self.tick_armed()
            }
            // A fetch landed: fold it through the transition machine, stamping
            // the last-good time from the current clock.
            Input::App(BoardMsg::Fetched(result)) => {
                let now_hhmm = hhmm(&self.now_iso);
                self.state = next_state(
                    std::mem::take(&mut self.state),
                    result,
                    self.now_unix,
                    &now_hhmm,
                );
                Vec::new()
            }
            // The visibility gate: forward down the command lane so the poll
            // task parks/resumes. A dropped receiver (session tearing down) is
            // fine to ignore.
            Input::SlotVisible(visible) => {
                let _ = self.cmd_tx.send(Cmd::SetVisible(visible));
                Vec::new()
            }
            // A tap on a departure row's arm Button (#236): its id is
            // `arm:<trip_id>`. A click toggles that train armed/disarmed; the
            // nudge itself only ever fires from a clock tick, never from the tap.
            Input::Event { node, kind } => {
                if kind == EventKind::Click
                    && let Some(trip_id) = node.strip_prefix("arm:")
                {
                    self.toggle_arm(trip_id);
                }
                Vec::new()
            }
            // Additive `Input` variants — today `EffectResult` (this board issues
            // no `RunCommand`), plus any future kind. A wildcard keeps new
            // variants from breaking the build.
            _ => Vec::new(),
        }
    }

    fn view(&self) -> View {
        let children = match &self.state {
            BoardState::Loading => {
                vec![status_text("loading departures…", "ts-departures-loading")]
            }
            BoardState::Err { err } => {
                vec![status_text(&error_text(err), "ts-departures-error")]
            }
            BoardState::Ok { items, .. } | BoardState::Stale { items, .. } => {
                let mut kids = Vec::new();
                if items.is_empty() {
                    kids.push(status_text(
                        "no matching S-Bahn departures right now",
                        "ts-departures-empty",
                    ));
                } else {
                    // Prune already-departed rows, same as the native clock
                    // tick's per-row `set_visible(false)`, so the open board
                    // doesn't keep showing past trains between fetches.
                    for r in items
                        .iter()
                        .filter(|r| !departed(self.now_unix, r.actual_unix))
                    {
                        kids.push(row_node(r, self.now_unix, self.armed.as_deref()));
                    }
                }
                if let BoardState::Stale { err, at_hhmm, .. } = &self.state {
                    kids.push(status_text(
                        &format!("· stale (last good {at_hhmm} — {err})"),
                        "ts-departures-stale-footer",
                    ));
                }
                kids
            }
        };
        Node::ListBox {
            id: Some(ROOT_ID.to_owned()),
            classes: vec!["ts-departures".to_owned()],
            children,
        }
        .into()
    }
}

// ── Arm-a-train helpers (#236) ───────────────────────────────────────────────

impl Board {
    /// Toggle the armed train on a row tap. Tapping the already-armed row
    /// disarms it; tapping any other row arms that trip and seeds the slack
    /// baseline from the **live** row (so the first tick diffs against it rather
    /// than firing on arm).
    fn toggle_arm(&mut self, trip_id: &str) {
        if self.armed.as_deref() == Some(trip_id) {
            self.disarm();
            return;
        }
        self.armed = Some(trip_id.to_owned());
        let now = self.now_unix;
        self.armed_prev_slack = self
            .armed_row()
            .map(|r| slack_secs(now, r.actual_unix, r.walk_minutes));
    }

    /// Clear the armed train and its slack baseline.
    fn disarm(&mut self) {
        self.armed = None;
        self.armed_prev_slack = None;
    }

    /// The live row matching the armed `trip_id`, resolved against the **current**
    /// items each call — never a frozen arm-time copy. This is the stale-snapshot
    /// guard: a refresh that moves the trip's time (a delay) is reflected because
    /// we always re-find by id.
    fn armed_row(&self) -> Option<&Row> {
        let armed = self.armed.as_deref()?;
        let items = match &self.state {
            BoardState::Ok { items, .. } | BoardState::Stale { items, .. } => items,
            BoardState::Loading | BoardState::Err { .. } => return None,
        };
        items.iter().find(|r| r.trip_id == armed)
    }

    /// Re-evaluate the armed train each clock tick against the live row set and
    /// raise a leave-by OSD nudge on a threshold crossing. Auto-disarms when the
    /// trip departs or drops out of the row set. Returns the effect(s) for
    /// `update` to hand back to the host (empty when nothing is armed or no
    /// threshold was crossed).
    fn tick_armed(&mut self) -> Vec<Effect> {
        if self.armed.is_none() {
            return Vec::new();
        }
        // Pull everything we need as owned values off the live row before any
        // mutation, so the immutable `armed_row` borrow is released.
        let resolved = self.armed_row().map(|r| {
            (
                slack_secs(self.now_unix, r.actual_unix, r.walk_minutes),
                departed(self.now_unix, r.actual_unix),
                leave_by_body(r),
            )
        });
        let Some((curr_slack, has_departed, body)) = resolved else {
            // The armed trip dropped out of the feed → disarm.
            self.disarm();
            return Vec::new();
        };
        if has_departed {
            self.disarm();
            return Vec::new();
        }
        let event = detect_leave_by_crossing(self.armed_prev_slack, curr_slack);
        self.armed_prev_slack = Some(curr_slack);
        match event {
            Some(ev) => vec![leave_by_effect(ev, &body)],
            None => Vec::new(),
        }
    }
}

/// One status/message line. A wrapping `Text` (not a `Label`) so a long
/// message can't force the sidebar surface wider than 320 px (the pet's #281
/// blow-out); the wrap is bounded by the container.
fn status_text(text: &str, class: &str) -> Node {
    Node::Text {
        id: None,
        text: text.to_owned(),
        max_width_chars: None,
        // Vocabulary gained `ellipsize` (#297); keep this line wrapping exactly as
        // before. Adopting single-line ellipsis for the destination is a follow-up.
        ellipsize: false,
        classes: vec![class.to_owned()],
    }
}

/// Build one departure row: a `Row` (horizontal) of line badge, destination,
/// the `{token} · HH:MM` time cell, and an optional delay badge, **wrapped in an
/// arm Button** (#236) so a tap arms/disarms this train. Mirrors the native
/// row's structural classes verbatim; the leave-by fade and the cancelled strike
/// ride the same `ts-*` classes. `armed` is the currently-armed `trip_id`, if
/// any — the matching row gains `ts-departure-armed`.
fn row_node(r: &Row, now_unix: i64, armed: Option<&str>) -> Node {
    let mut classes = vec!["ts-departure-row".to_owned()];
    if r.cancelled {
        classes.push("ts-cancelled".to_owned());
    }
    let (token, unreachable) = lead_label(now_unix, r.actual_unix, r.walk_minutes);
    if unreachable {
        classes.push("ts-departure-unreachable".to_owned());
    }
    if armed == Some(r.trip_id.as_str()) {
        classes.push("ts-departure-armed".to_owned());
    }

    let mut children = vec![
        // Line badge — the safe-line class carries its route color.
        Node::Label {
            id: None,
            text: r.line.clone(),
            classes: vec![
                "ts-line-badge".to_owned(),
                format!("ts-line-{}", safe_line(&r.line)),
            ],
        },
        // Destination — wrapping `Text` (capped at 22 chars) so a long name
        // wraps within the sidebar rather than forcing it wider.
        Node::Text {
            id: None,
            text: r.direction.clone(),
            max_width_chars: Some(DIRECTION_MAX_CHARS),
            // Behaviour-preserving compile-fix for the #297 `ellipsize` field.
            // Switching this to `true` (single-line ellipsis, native-board parity)
            // is the adoption follow-up (#296) — not this vocabulary-only PR.
            ellipsize: false,
            classes: vec!["ts-departure-direction".to_owned()],
        },
        // Time cell: the leave-by / departs-in token plus the local HH:MM.
        Node::Label {
            id: None,
            text: format!("{token} · {}", r.hhmm),
            classes: vec!["ts-departure-time".to_owned()],
        },
    ];
    if let Some(text) = delay_string(r.delay_minutes) {
        children.push(Node::Label {
            id: None,
            text,
            classes: vec!["ts-departure-delay".to_owned()],
        });
    }

    let row = Node::Row {
        id: None,
        classes,
        children,
    };
    // Wrap the row in a Button so it becomes a click target (#236). A `Row` isn't
    // an event target, but a `Button` opts into the `Click` event by vocabulary —
    // no manifest/StateKey change. The id carries the `trip_id` so `update` can
    // resolve the tap back to the armed trip; `ts-departure-arm` flattens the
    // default button chrome back to a plain, full-width list row.
    Node::Button {
        id: format!("arm:{}", r.trip_id),
        classes: vec!["ts-departure-arm".to_owned()],
        child: Box::new(row),
    }
}

fn main() {
    hytte_plugin::run::<Board>();
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::DateTime;

    /// Unix seconds for an RFC 3339 instant — the test analogue of the native
    /// widget's `at(h, m, s)` helper, timezone-pinned so it doesn't shift with
    /// the machine tz.
    fn ts(rfc3339: &str) -> i64 {
        DateTime::parse_from_rfc3339(rfc3339)
            .expect("valid rfc3339")
            .timestamp()
    }

    /// A fixed reference "now": 2030-01-01T16:00:00+01:00.
    fn now() -> i64 {
        ts("2030-01-01T16:00:00+01:00")
    }

    fn sample_row(line: &str, direction: &str, actual: &str, walk_minutes: u32) -> Row {
        Row {
            line: line.to_owned(),
            direction: direction.to_owned(),
            hhmm: hhmm(actual),
            actual_unix: ts(actual),
            delay_minutes: 0,
            cancelled: false,
            trip_id: format!("trip-{line}"),
            walk_minutes,
        }
    }

    fn fresh() -> (Board, CmdReceiver<Cmd>) {
        let (tx, rx) = hytte_plugin::cmd_channel();
        (Board::init(tx), rx)
    }

    // ── relative_label / lead_label (leave-by) — ported edge cases ────────────

    #[test]
    fn relative_label_within_60s_is_now_and_past_is_now() {
        assert_eq!(
            relative_label(now(), ts("2030-01-01T16:00:30+01:00")),
            "now"
        );
        assert_eq!(
            relative_label(ts("2030-01-01T16:00:30+01:00"), now()),
            "now"
        );
    }

    #[test]
    fn relative_label_rounds_at_the_30s_boundary() {
        // 7m31s rounds up to 8; 7m29s rounds down to 7; 61s is 1 min.
        assert_eq!(
            relative_label(now(), ts("2030-01-01T16:07:31+01:00")),
            "8 min"
        );
        assert_eq!(
            relative_label(now(), ts("2030-01-01T16:07:29+01:00")),
            "7 min"
        );
        assert_eq!(
            relative_label(now(), ts("2030-01-01T16:01:01+01:00")),
            "1 min"
        );
    }

    #[test]
    fn lead_label_zero_walk_is_plain_relative() {
        assert_eq!(
            lead_label(now(), ts("2030-01-01T16:07:00+01:00"), 0),
            ("7 min".to_owned(), false)
        );
        assert_eq!(
            lead_label(now(), ts("2030-01-01T16:00:30+01:00"), 0),
            ("now".to_owned(), false)
        );
    }

    #[test]
    fn lead_label_counts_down_slack_and_flags_unreachable() {
        // 14 min out, 10 min walk → 4 min of slack.
        assert_eq!(
            lead_label(now(), ts("2030-01-01T16:14:00+01:00"), 10),
            ("4 min".to_owned(), false)
        );
        // 11 min out, 10 min walk → 1 min slack.
        assert_eq!(
            lead_label(now(), ts("2030-01-01T16:11:00+01:00"), 10),
            ("1 min".to_owned(), false)
        );
        // Exactly the walk window: leave now, still catchable (not faded).
        assert_eq!(
            lead_label(now(), ts("2030-01-01T16:10:00+01:00"), 10),
            ("now".to_owned(), false)
        );
        // 3 min out, 10 min walk → already missed: "now" + faded.
        assert_eq!(
            lead_label(now(), ts("2030-01-01T16:03:00+01:00"), 10),
            ("now".to_owned(), true)
        );
    }

    // ── departed prune ────────────────────────────────────────────────────────

    #[test]
    fn departed_hides_only_after_grace() {
        let train = ts("2030-01-01T16:00:00+01:00");
        assert!(!departed(ts("2030-01-01T15:59:00+01:00"), train));
        assert!(!departed(ts("2030-01-01T16:00:00+01:00"), train));
        assert!(!departed(ts("2030-01-01T16:00:30+01:00"), train)); // within grace
        assert!(departed(ts("2030-01-01T16:00:31+01:00"), train)); // past it
    }

    // ── delay_string ──────────────────────────────────────────────────────────

    #[test]
    fn delay_string_only_surfaces_lateness() {
        assert_eq!(delay_string(0), None);
        assert_eq!(delay_string(-2), None);
        assert_eq!(delay_string(5), Some("+5".to_owned()));
    }

    // ── hhmm / error_text ─────────────────────────────────────────────────────

    #[test]
    fn hhmm_slices_the_time_and_tolerates_garbage() {
        assert_eq!(hhmm("2026-07-11T15:49:00+02:00"), "15:49");
        assert_eq!(hhmm("short"), "--:--");
        assert_eq!(hhmm(""), "--:--");
    }

    #[test]
    fn error_text_wraps_fetch_errors_but_shows_config_hints_plainly() {
        assert_eq!(
            error_text("http: connection refused"),
            "can't reach BVG: http: connection refused"
        );
        assert_eq!(
            error_text("decode: bad json"),
            "can't reach BVG: decode: bad json"
        );
        // A no-station hint isn't a reachability problem — no prefix.
        let hint = "no departures station configured — set `station` in places.toml";
        assert_eq!(error_text(hint), hint);
    }

    // ── next_state — ported transition table ─────────────────────────────────

    #[test]
    fn next_state_ok_replaces_anything() {
        let next = next_state(
            BoardState::Err {
                err: "boom".to_owned(),
            },
            Ok(vec![sample_row(
                "S9",
                "Spandau",
                "2030-01-01T16:05:00+01:00",
                0,
            )]),
            now(),
            "16:00",
        );
        match next {
            BoardState::Ok {
                at_unix,
                at_hhmm,
                items,
            } => {
                assert_eq!(at_unix, now());
                assert_eq!(at_hhmm, "16:00");
                assert_eq!(items.len(), 1);
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    #[test]
    fn next_state_ok_then_err_becomes_stale_and_recovers() {
        let items = vec![sample_row("S9", "Spandau", "2030-01-01T16:05:00+01:00", 0)];
        let ok = BoardState::Ok {
            at_unix: now(),
            at_hhmm: "16:00".to_owned(),
            items: items.clone(),
        };
        // 10 min later, below the drop threshold → Stale, keeping the old list.
        let stale = next_state(ok, Err("net".to_owned()), now() + 10 * 60, "16:10");
        let BoardState::Stale {
            at_hhmm,
            items: kept,
            err,
            ..
        } = &stale
        else {
            panic!("expected Stale, got {stale:?}");
        };
        assert_eq!(at_hhmm, "16:00", "keeps the last-good time");
        assert_eq!(kept.len(), 1);
        assert_eq!(err, "net");
        // A later success recovers to Ok.
        let recovered = next_state(
            stale,
            Ok(vec![sample_row(
                "S46",
                "Königs Wusterhausen",
                "2030-01-01T16:20:00+01:00",
                0,
            )]),
            now() + 15 * 60,
            "16:15",
        );
        assert!(matches!(recovered, BoardState::Ok { .. }));
    }

    #[test]
    fn next_state_stale_drops_to_err_at_the_threshold() {
        let stale = BoardState::Stale {
            at_unix: now(),
            at_hhmm: "16:00".to_owned(),
            items: vec![sample_row("S9", "Spandau", "2030-01-01T16:05:00+01:00", 0)],
            err: "old".to_owned(),
        };
        // Exactly 30 min later — the `>=` comparison must drop.
        let dropped = next_state(stale, Err("still net".to_owned()), now() + 30 * 60, "16:30");
        assert!(matches!(dropped, BoardState::Err { .. }));
    }

    #[test]
    fn next_state_loading_or_err_on_error_stays_err() {
        assert!(matches!(
            next_state(BoardState::Loading, Err("boom".to_owned()), now(), "16:00"),
            BoardState::Err { .. }
        ));
        let next = next_state(
            BoardState::Err {
                err: "old".to_owned(),
            },
            Err("new".to_owned()),
            now(),
            "16:00",
        );
        match next {
            BoardState::Err { err } => assert_eq!(err, "new"),
            other => panic!("expected Err, got {other:?}"),
        }
    }

    #[test]
    fn trip_id_is_preserved_through_a_refresh_fold() {
        // A refresh replaces the list; the new rows keep their HAFAS trip_ids
        // (stable across refreshes — the anchor #236's arm-a-train folds onto).
        let (mut board, _rx) = fresh();
        let first = sample_row("S9", "Spandau", "2030-01-01T16:05:00+01:00", 0);
        board.update(Input::App(BoardMsg::Fetched(Ok(vec![first]))));
        let mut refreshed = sample_row("S9", "Spandau", "2030-01-01T16:08:00+01:00", 0);
        refreshed.trip_id = "trip-stable-across-refresh".to_owned();
        board.update(Input::App(BoardMsg::Fetched(Ok(vec![refreshed]))));
        let BoardState::Ok { items, .. } = &board.state else {
            panic!("expected Ok");
        };
        assert_eq!(items[0].trip_id, "trip-stable-across-refresh");
    }

    // ── The reducer: snapshot, visibility gate ───────────────────────────────

    #[test]
    fn snapshot_updates_the_clock() {
        let (mut board, _rx) = fresh();
        board.update(Input::Snapshot(hytte_plugin::proto::StateSnapshot {
            clock: Some(hytte_plugin::proto::ClockState {
                iso: "2030-01-01T16:00:00+01:00".to_owned(),
                unix: 42,
            }),
        }));
        assert_eq!(board.now_unix, 42);
        assert_eq!(board.now_iso, "2030-01-01T16:00:00+01:00");
    }

    #[test]
    fn visibility_flip_emits_the_gate_command_down_the_lane() {
        let (mut board, mut rx) = fresh();
        let fx = board.update(Input::SlotVisible(true));
        assert!(fx.is_empty(), "the board asks nothing of the shell");
        assert!(matches!(rx.try_recv(), Ok(Cmd::SetVisible(true))));
        board.update(Input::SlotVisible(false));
        assert!(matches!(rx.try_recv(), Ok(Cmd::SetVisible(false))));
    }

    #[test]
    fn manifest_subscribes_clock_and_slot_visible() {
        // The board gates its poll task on visibility, so it MUST subscribe
        // `SlotVisible` — the push is opt-in via the manifest (#305), so without
        // the subscription the host would never send the edges the gate needs.
        let m = Board::manifest();
        assert_eq!(m.mount, Mount::SidebarBottom);
        assert!(
            m.subscribes.contains(&StateKey::Clock),
            "Clock drives the per-second relabel"
        );
        assert!(
            m.subscribes.contains(&StateKey::SlotVisible),
            "SlotVisible opts the board into the visibility push it gates on (#305)"
        );
    }

    // ── view: every state renders the expected tree ──────────────────────────

    fn root_children(board: &Board) -> Vec<Node> {
        let Node::ListBox {
            classes, children, ..
        } = board.view().tree
        else {
            panic!("root is a ListBox");
        };
        assert_eq!(classes, vec!["ts-departures".to_owned()]);
        children
    }

    /// Unwrap the arm Button that now wraps every departure row (#236) → its
    /// inner `Node::Row`. The tap wrapper is transparent to the row-shape asserts.
    fn row_of(node: &Node) -> &Node {
        let Node::Button { child, .. } = node else {
            panic!("a departure row is wrapped in an arm Button");
        };
        child
    }

    /// A clock `Input::Snapshot` at `rfc3339` — the arm-a-train tick trigger.
    fn snapshot_at(rfc3339: &str) -> Input<BoardMsg> {
        Input::Snapshot(hytte_plugin::proto::StateSnapshot {
            clock: Some(hytte_plugin::proto::ClockState {
                iso: rfc3339.to_owned(),
                unix: ts(rfc3339),
            }),
        })
    }

    #[test]
    fn view_loading_is_a_single_status_line() {
        let (board, _rx) = fresh();
        let kids = root_children(&board);
        assert_eq!(kids.len(), 1);
        assert!(matches!(
            &kids[0],
            Node::Text { text, classes, .. }
                if text == "loading departures…" && classes == &["ts-departures-loading"]
        ));
    }

    #[test]
    fn view_no_station_error_is_shown_plainly() {
        let (mut board, _rx) = fresh();
        board.update(Input::App(BoardMsg::Fetched(Err(
            "no departures station configured — set `station` in places.toml".to_owned(),
        ))));
        let kids = root_children(&board);
        assert!(matches!(
            &kids[0],
            Node::Text { text, classes, .. }
                if text.starts_with("no departures station configured")
                    && classes == &["ts-departures-error"]
        ));
    }

    #[test]
    fn view_data_renders_rows_and_omits_departed() {
        let (mut board, _rx) = fresh();
        board.now_unix = now();
        board.now_iso = "2030-01-01T16:00:00+01:00".to_owned();
        let mut cancelled = sample_row("S8", "Wildau", "2030-01-01T16:12:00+01:00", 0);
        cancelled.cancelled = true;
        cancelled.delay_minutes = 3;
        board.update(Input::App(BoardMsg::Fetched(Ok(vec![
            sample_row("S9", "Spandau", "2030-01-01T16:05:00+01:00", 0),
            cancelled,
            // Already departed (past the 30 s grace): must be pruned from view.
            sample_row("S46", "Gone", "2030-01-01T15:50:00+01:00", 0),
        ]))));

        let kids = root_children(&board);
        assert_eq!(kids.len(), 2, "the departed row is pruned");

        // First row: badge S9 + destination + time, no delay badge. Each row is
        // wrapped in an arm Button now (#236), so unwrap before shape-asserting.
        let Node::Row {
            classes, children, ..
        } = row_of(&kids[0])
        else {
            panic!("row is a Row");
        };
        assert_eq!(classes, &["ts-departure-row".to_owned()]);
        assert!(matches!(
            &children[0],
            Node::Label { text, classes, .. }
                if text == "S9" && classes.contains(&"ts-line-S9".to_owned())
        ));
        assert!(matches!(
            &children[1],
            Node::Text { text, classes, .. }
                if text == "Spandau" && classes == &["ts-departure-direction"]
        ));
        assert!(matches!(
            &children[2],
            Node::Label { text, classes, .. }
                if text == "5 min · 16:05" && classes == &["ts-departure-time"]
        ));
        assert_eq!(children.len(), 3, "on-time row has no delay badge");

        // Second row: cancelled → ts-cancelled class + a delay badge.
        let Node::Row {
            classes, children, ..
        } = row_of(&kids[1])
        else {
            panic!("row is a Row");
        };
        assert!(classes.contains(&"ts-cancelled".to_owned()));
        assert!(matches!(
            children.last(),
            Some(Node::Label { text, classes, .. })
                if text == "+3" && classes == &["ts-departure-delay"]
        ));
    }

    #[test]
    fn view_unreachable_row_is_faded() {
        let (mut board, _rx) = fresh();
        board.now_unix = now();
        // 3 min out, 10 min walk → unreachable → ts-departure-unreachable.
        board.update(Input::App(BoardMsg::Fetched(Ok(vec![sample_row(
            "S9",
            "Spandau",
            "2030-01-01T16:03:00+01:00",
            10,
        )]))));
        let kids = root_children(&board);
        let Node::Row { classes, .. } = row_of(&kids[0]) else {
            panic!("row is a Row");
        };
        assert!(classes.contains(&"ts-departure-unreachable".to_owned()));
    }

    #[test]
    fn view_empty_items_shows_the_empty_line() {
        let (mut board, _rx) = fresh();
        board.update(Input::App(BoardMsg::Fetched(Ok(Vec::new()))));
        let kids = root_children(&board);
        assert_eq!(kids.len(), 1);
        assert!(matches!(
            &kids[0],
            Node::Text { classes, .. } if classes == &["ts-departures-empty"]
        ));
    }

    #[test]
    fn view_stale_shows_rows_then_a_footer() {
        let (mut board, _rx) = fresh();
        board.now_unix = now();
        board.now_iso = "2030-01-01T16:00:00+01:00".to_owned();
        board.update(Input::App(BoardMsg::Fetched(Ok(vec![sample_row(
            "S9",
            "Spandau",
            "2030-01-01T16:05:00+01:00",
            0,
        )]))));
        // A later failure, within the drop window → Stale (rows + footer).
        board.now_unix = now() + 5 * 60;
        board.update(Input::App(BoardMsg::Fetched(Err("http: down".to_owned()))));
        let kids = root_children(&board);
        assert_eq!(kids.len(), 2, "one row + the stale footer");
        assert!(matches!(row_of(&kids[0]), Node::Row { .. }));
        assert!(matches!(
            &kids[1],
            Node::Text { text, classes, .. }
                if text.contains("stale (last good 16:00") && classes == &["ts-departures-stale-footer"]
        ));
    }

    // ── Arm-a-train leave-by nudge (#236) ─────────────────────────────────────

    #[test]
    fn manifest_declares_raise_osd_capability() {
        // Arm-a-train brokers `Effect::RaiseOsd`, so the manifest must request the
        // matching capability.
        let m = Board::manifest();
        assert!(
            m.capabilities.contains(&Capability::RaiseOsd),
            "the board raises OSD nudges, so it requests the RaiseOsd cap"
        );
    }

    #[test]
    fn detect_leave_by_crossing_fires_once_per_threshold() {
        // No baseline (just armed) never fires — it only seeds the diff.
        assert_eq!(detect_leave_by_crossing(None, 500), None);
        // Slack falling through 3 min fires Soon…
        assert_eq!(
            detect_leave_by_crossing(Some(181), 179),
            Some(LeaveByEvent::Soon)
        );
        // …and a steady state already below 3 min re-fires nothing (latched).
        assert_eq!(detect_leave_by_crossing(Some(179), 120), None);
        // Slack hitting now/0 fires Now…
        assert_eq!(
            detect_leave_by_crossing(Some(1), -1),
            Some(LeaveByEvent::Now)
        );
        // …and staying below 0 re-fires nothing.
        assert_eq!(detect_leave_by_crossing(Some(-1), -30), None);
        // A single tick spanning both thresholds fires the more urgent (Now).
        assert_eq!(
            detect_leave_by_crossing(Some(500), -5),
            Some(LeaveByEvent::Now)
        );
    }

    /// Arm the sole trip on the board and return the board for further ticks.
    fn armed_board(actual: &str, walk: u32) -> Board {
        let (mut board, _rx) = fresh();
        board.now_unix = now();
        board.now_iso = "2030-01-01T16:00:00+01:00".to_owned();
        let mut r = sample_row("S9", "Spandau", actual, walk);
        r.trip_id = "trip-armed".to_owned();
        board.update(Input::App(BoardMsg::Fetched(Ok(vec![r]))));
        board.update(Input::Event {
            node: "arm:trip-armed".to_owned(),
            kind: EventKind::Click,
        });
        board
    }

    #[test]
    fn tap_arms_then_taps_disarms() {
        let mut board = armed_board("2030-01-01T16:14:00+01:00", 10);
        assert_eq!(board.armed.as_deref(), Some("trip-armed"));
        assert!(
            board.armed_prev_slack.is_some(),
            "arming seeds the slack baseline from the live row"
        );
        // A second tap on the same row disarms.
        let fx = board.update(Input::Event {
            node: "arm:trip-armed".to_owned(),
            kind: EventKind::Click,
        });
        assert!(fx.is_empty(), "toggling asks the shell for nothing");
        assert_eq!(board.armed, None);
        assert_eq!(board.armed_prev_slack, None);
    }

    #[test]
    fn armed_row_gets_the_class() {
        let board = armed_board("2030-01-01T16:14:00+01:00", 10);
        let kids = root_children(&board);
        // The row is wrapped in an arm Button keyed by trip_id; the inner Row
        // carries the armed class.
        let Node::Button { id, child, .. } = &kids[0] else {
            panic!("row is wrapped in an arm Button");
        };
        assert_eq!(id, "arm:trip-armed");
        let Node::Row { classes, .. } = child.as_ref() else {
            panic!("the button child is the Row");
        };
        assert!(classes.contains(&"ts-departure-armed".to_owned()));
    }

    #[test]
    fn armed_resolves_against_live_rows() {
        // Arm a trip, then a refresh MOVES its departure time (a delay lands). The
        // armed lookup must reflect the NEW time — we resolve by trip_id against
        // the live rows each tick, never a frozen arm-time copy (stale-snapshot
        // guard).
        let mut board = armed_board("2030-01-01T16:14:00+01:00", 10);
        let mut moved = sample_row("S9", "Spandau", "2030-01-01T16:19:00+01:00", 10);
        moved.trip_id = "trip-armed".to_owned();
        board.update(Input::App(BoardMsg::Fetched(Ok(vec![moved]))));
        let row = board.armed_row().expect("armed trip still on the board");
        assert_eq!(row.actual_unix, ts("2030-01-01T16:19:00+01:00"));
        assert_eq!(row.hhmm, "16:19");
    }

    #[test]
    fn unarmed_emits_no_effect() {
        let (mut board, _rx) = fresh();
        board.now_unix = now();
        board.update(Input::App(BoardMsg::Fetched(Ok(vec![sample_row(
            "S9",
            "Spandau",
            "2030-01-01T16:03:00+01:00",
            10,
        )]))));
        // A clock tick with nothing armed asks the shell for nothing.
        let fx = board.update(snapshot_at("2030-01-01T16:03:30+01:00"));
        assert!(fx.is_empty());
    }

    #[test]
    fn armed_snapshot_crossing_raises_leave_by_osd() {
        // 16:14 departure, 10 min walk → slack 4 min at arm (16:00). One minute
        // on, slack falls to 3 min → the "leave soon" crossing.
        let mut board = armed_board("2030-01-01T16:14:00+01:00", 10);
        let fx = board.update(snapshot_at("2030-01-01T16:01:00+01:00"));
        assert_eq!(fx.len(), 1, "the 3-min crossing raises one nudge");
        let Effect::RaiseOsd { title, body, icon } = &fx[0] else {
            panic!("expected a RaiseOsd effect, got {:?}", fx[0]);
        };
        assert_eq!(title, "Leave soon");
        assert_eq!(body, "S9 · Spandau · 16:14");
        assert_eq!(icon.as_deref(), Some("appointment-soon-symbolic"));
        // The next tick, still below 3 min, latches — no re-fire.
        let fx2 = board.update(snapshot_at("2030-01-01T16:01:30+01:00"));
        assert!(
            fx2.is_empty(),
            "a steady state below the threshold re-fires nothing"
        );
    }

    #[test]
    fn armed_auto_disarms_when_trip_leaves_the_board() {
        let mut board = armed_board("2030-01-01T16:14:00+01:00", 10);
        assert_eq!(board.armed.as_deref(), Some("trip-armed"));
        // A refresh where the armed trip has dropped off the board entirely.
        board.update(Input::App(BoardMsg::Fetched(Ok(vec![sample_row(
            "S46",
            "Gone",
            "2030-01-01T16:20:00+01:00",
            0,
        )]))));
        // The next tick resolves against the live rows, finds it missing, disarms.
        let fx = board.update(snapshot_at("2030-01-01T16:05:00+01:00"));
        assert!(fx.is_empty());
        assert_eq!(board.armed, None, "a vanished armed trip auto-disarms");
    }

    /// Reference-frame check: unix arithmetic is timezone-free, so `now()` and
    /// the sample instants sit on the same absolute line regardless of the
    /// machine's `Local` (there's no tzdata under `nix build`).
    #[test]
    fn reference_now_precedes_the_sample_departures() {
        assert!(now() < ts("2030-01-01T16:05:00+01:00"));
    }
}
