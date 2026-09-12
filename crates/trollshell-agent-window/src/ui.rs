//! The widgets: the header line, the lifecycle buttons, and the settings page.
//!
//! Everything here is a `set_*` call driven by [`crate::chrome`]'s models, so
//! the rules live where a hermetic test can reach them and this layer is what
//! one display test pins.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;

use crate::chrome::{Controls, Fact, HeaderModel};

/// The lifecycle verb a button press asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Press {
    /// Start the container.
    Start,
    /// Stop it, gracefully.
    Stop,
    /// Park or resume the turn loop.
    SetPaused(bool),
}

/// The header line: who this window is for, what the hive says about them, and
/// the three verbs.
///
/// It lives **above** the `ViewStack`, so it is on screen on both tabs — which
/// is why the settings page carries no second copy of the buttons.
pub struct Header {
    /// The widget to pack.
    pub root: gtk::Box,
    icon: gtk::Image,
    title: gtk::Label,
    model: gtk::Label,
    status_icon: gtk::Image,
    status: gtk::Label,
    start: gtk::Button,
    stop: gtk::Button,
    pause: gtk::ToggleButton,
    /// The status class currently on the label, so the next apply can take it
    /// off again — `add_css_class` does not replace.
    applied_class: RefCell<String>,
    /// Set while [`Header::apply`] moves the pause toggle, so the programmatic
    /// `set_active` does not read back as a click. `toggled` fires either way,
    /// and without this the first poll after a pause would send `SetPaused`
    /// again, forever.
    echo: Rc<Cell<bool>>,
}

impl Header {
    /// Build the header. Nothing is connected yet — see [`Header::connect`].
    #[must_use]
    pub fn new() -> Self {
        let icon = gtk::Image::builder().pixel_size(24).build();
        let title = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        title.add_css_class("title-4");
        let model = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        model.add_css_class("dim-label");
        model.add_css_class("caption");

        let status_icon = gtk::Image::builder().pixel_size(12).build();
        // One line, ellipsized, with the full text on hover — the card's rule
        // for the harness's own status, which can be a paragraph.
        let status = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .single_line_mode(true)
            .hexpand(true)
            .build();
        status.add_css_class("caption");

        let start = icon_button("media-playback-start-symbolic", "Start this agent");
        let stop = icon_button("media-playback-stop-symbolic", "Stop this agent");
        let pause = gtk::ToggleButton::builder()
            .icon_name("media-playback-pause-symbolic")
            .tooltip_text("Park the turn loop")
            .build();
        pause.add_css_class("flat");

        let identity = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        identity.append(&title);
        identity.append(&model);

        let statusline = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        statusline.append(&status_icon);
        statusline.append(&status);

        let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        text.set_hexpand(true);
        text.set_valign(gtk::Align::Center);
        text.append(&identity);
        text.append(&statusline);

        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        buttons.set_valign(gtk::Align::Center);
        buttons.append(&start);
        buttons.append(&stop);
        buttons.append(&pause);

        let root = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        root.set_margin_top(8);
        root.set_margin_bottom(8);
        root.set_margin_start(12);
        root.set_margin_end(12);
        root.append(&icon);
        root.append(&text);
        root.append(&buttons);

        Self {
            root,
            icon,
            title,
            model,
            status_icon,
            status,
            start,
            stop,
            pause,
            applied_class: RefCell::new(String::new()),
            echo: Rc::new(Cell::new(false)),
        }
    }

    /// Route every button press to `on_press`.
    pub fn connect(&self, on_press: impl Fn(Press) + 'static) {
        let on_press = Rc::new(on_press);
        let f = Rc::clone(&on_press);
        self.start.connect_clicked(move |_| f(Press::Start));
        let f = Rc::clone(&on_press);
        self.stop.connect_clicked(move |_| f(Press::Stop));
        let echo = Rc::clone(&self.echo);
        self.pause.connect_toggled(move |b| {
            if echo.get() {
                return;
            }
            on_press(Press::SetPaused(b.is_active()));
        });
    }

    /// Show one state.
    pub fn apply(&self, h: &HeaderModel, c: &Controls) {
        self.icon.set_icon_name(Some(&h.icon));
        self.title.set_text(&h.title);
        self.title.set_tooltip_text(Some(&h.title));

        self.model.set_text(h.model_word.as_deref().unwrap_or(""));
        self.model.set_visible(h.model_word.is_some());
        self.model.set_tooltip_text(h.model_full.as_deref());

        self.status_icon.set_icon_name(Some(&h.status_icon));
        self.status.set_text(&h.status);
        self.status.set_tooltip_text(Some(&h.status));

        let mut applied = self.applied_class.borrow_mut();
        if *applied != h.status_class {
            if !applied.is_empty() {
                self.status.remove_css_class(&applied);
                self.status_icon.remove_css_class(&applied);
            }
            self.status.add_css_class(&h.status_class);
            self.status_icon.add_css_class(&h.status_class);
            applied.clone_from(&h.status_class);
        }

        self.start.set_visible(!c.live || c.can_start);
        self.start.set_sensitive(c.live && c.can_start);
        self.stop.set_visible(c.live && c.can_stop);
        self.stop.set_sensitive(c.live && c.can_stop);
        self.pause.set_sensitive(c.live);
        if self.pause.is_active() != c.paused {
            self.echo.set(true);
            self.pause.set_active(c.paused);
            self.echo.set(false);
        }
    }

    /// Press Start, **refusing an insensitive button** — what a user can
    /// actually do.
    ///
    /// The two `press_*_for_test` helpers exist so `window.rs`'s display tests
    /// can drive the wiring between a button and the command lane, which
    /// nothing covered before #1130's review (L2).
    ///
    /// They **panic** rather than press a dead button, because `emit_clicked`
    /// does not care: it fires the `clicked` handler regardless of
    /// `sensitive`, so a helper that used it alone would let
    /// `a_button_press_reaches_the_command_lane` pass with every
    /// `set_sensitive` line in [`Header::apply`] deleted — measured on the
    /// #1130 re-verification (N2). GTK will not deliver a click to an
    /// insensitive widget, so neither will this.
    #[cfg(all(test, feature = "system-tests"))]
    pub fn press_start_for_test(&self) {
        let (visible, sensitive) = own_flags(&self.start);
        assert!(
            visible && sensitive,
            "Start is not pressable (visible: {visible}, sensitive: {sensitive}); a user could \
             not have done this"
        );
        self.start.emit_clicked();
    }

    /// Press Stop. See [`Header::press_start_for_test`].
    #[cfg(all(test, feature = "system-tests"))]
    pub fn press_stop_for_test(&self) {
        let (visible, sensitive) = own_flags(&self.stop);
        assert!(
            visible && sensitive,
            "Stop is not pressable (visible: {visible}, sensitive: {sensitive}); a user could \
             not have done this"
        );
        self.stop.emit_clicked();
    }

    /// `(visible, sensitive)` for Start, Stop and the pause toggle — the
    /// display tests' read-back for [`Header::apply`]'s effect on the buttons.
    #[cfg(all(test, feature = "system-tests"))]
    pub fn button_states(&self) -> [(bool, bool); 3] {
        [
            own_flags(&self.start),
            own_flags(&self.stop),
            own_flags(&self.pause),
        ]
    }

    /// Whether the pause toggle is down.
    #[cfg(all(test, feature = "system-tests"))]
    pub fn pause_is_active(&self) -> bool {
        self.pause.is_active()
    }

    /// The status line as shown.
    ///
    /// These read-backs exist only for the display tests, and carry their gate
    /// so a plain build does not compile an accessor nothing calls.
    #[cfg(all(test, feature = "system-tests"))]
    pub fn status_text(&self) -> String {
        self.status.text().to_string()
    }

    /// The title as shown.
    #[cfg(all(test, feature = "system-tests"))]
    pub fn title_text(&self) -> String {
        self.title.text().to_string()
    }

    /// The model chip as shown, `None` when it is hidden.
    #[cfg(all(test, feature = "system-tests"))]
    pub fn model_text(&self) -> Option<String> {
        self.model
            .is_visible()
            .then(|| self.model.text().to_string())
    }
}

impl Default for Header {
    fn default() -> Self {
        Self::new()
    }
}

/// A widget's **own** `visible`/`sensitive` flags — the two [`Header::apply`]
/// sets, read straight back off the properties.
///
/// Not `WidgetExt::is_visible`/`is_sensitive`: those are the *effective*
/// values, which fold in every ancestor. A window that is never presented is
/// not visible, so every button inside it reports `is_visible() == false`
/// however `apply` left it — which made the first version of the press
/// helpers refuse a perfectly live Start (measured, #1130 N2's own fix). The
/// own flag is what this layer controls and what a test should hold it to.
#[cfg(all(test, feature = "system-tests"))]
fn own_flags(w: &impl gtk::glib::object::ObjectExt) -> (bool, bool) {
    (
        w.property::<bool>("visible"),
        w.property::<bool>("sensitive"),
    )
}

fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    let b = gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .build();
    b.add_css_class("flat");
    b
}

/// The settings tab: what the hive says about this agent, read-only.
pub struct Settings {
    /// The widget to mount in the `ViewStack`.
    pub root: adw::PreferencesPage,
    agent: adw::PreferencesGroup,
    hive: adw::PreferencesGroup,
    agent_rows: RefCell<Vec<adw::ActionRow>>,
    hive_rows: RefCell<Vec<adw::ActionRow>>,
}

impl Settings {
    /// Build the empty page.
    #[must_use]
    pub fn new() -> Self {
        let agent = adw::PreferencesGroup::builder()
            .title("Agent")
            .description(
                "What the hive reports for this agent. Read-only in v1 — an agent's own \
                 configuration lives in its config flake, not in the shell.",
            )
            .build();
        let hive = adw::PreferencesGroup::builder()
            .title("Hive")
            .description("Where this window reads that from.")
            .build();
        let root = adw::PreferencesPage::new();
        root.add(&agent);
        root.add(&hive);
        Self {
            root,
            agent,
            hive,
            agent_rows: RefCell::new(Vec::new()),
            hive_rows: RefCell::new(Vec::new()),
        }
    }

    /// Replace both groups' rows.
    ///
    /// Rebuilt rather than updated in place: the row set is short and fixed,
    /// and a rebuild cannot leave a stale row behind when a field goes from
    /// present to absent.
    ///
    /// Rows are `adw::ActionRow`s — `GtkListBoxRow`s — because a
    /// `PreferencesGroup` renders anything else *below* its list rather than
    /// among the rows, which type-checks and looks wrong.
    pub fn apply(&self, agent: &[Fact], hive: &[Fact]) {
        for (group, tracked, facts) in [
            (&self.agent, &self.agent_rows, agent),
            (&self.hive, &self.hive_rows, hive),
        ] {
            // A group owns its rows through a `ListBox` it does not expose, so
            // removal goes through the group, never `unparent`.
            for row in tracked.borrow_mut().drain(..) {
                group.remove(&row);
            }
            let mut tracked = tracked.borrow_mut();
            for fact in facts {
                let row = adw::ActionRow::builder()
                    .title(fact.label)
                    .subtitle(&fact.value)
                    // The values are the hive's strings, not markup — a status
                    // text containing `<` would otherwise be parsed as a tag
                    // and silently swallow the rest of the line.
                    .use_markup(false)
                    .subtitle_selectable(true)
                    .build();
                // A hive URL or a status paragraph must not grow the window.
                row.set_subtitle_lines(2);
                group.add(&row);
                tracked.push(row);
            }
        }
    }

    /// The rows as this page **tracks** them — the bookkeeping, not the
    /// widget tree.
    ///
    /// Handed to the display tests so they can ask the *container* what became
    /// of a row after a rebuild. That distinction is the whole of #1130's M4:
    /// `row_text` below reads back from these same vectors, so it can only
    /// ever confirm the bookkeeping, and dropping the `group.remove(&row)`
    /// call while keeping the drain left it green while every old
    /// `ActionRow` leaked into the group — seven more rows per poll on a page
    /// that rebuilds on every state change. This is the #851 shape again:
    /// assert against the container, not against your own `Vec`.
    #[cfg(all(test, feature = "system-tests"))]
    pub fn tracked_rows(&self) -> Vec<adw::ActionRow> {
        self.agent_rows
            .borrow()
            .iter()
            .chain(self.hive_rows.borrow().iter())
            .cloned()
            .collect()
    }

    /// Every row's `title: subtitle` — the display tests' read-back, and so
    /// carrying their gate.
    #[cfg(all(test, feature = "system-tests"))]
    pub fn row_text(&self) -> Vec<String> {
        self.agent_rows
            .borrow()
            .iter()
            .chain(self.hive_rows.borrow().iter())
            .map(|r| format!("{}: {}", r.title(), r.subtitle().unwrap_or_default()))
            .collect()
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self::new()
    }
}

/// The banner shown when a verb was refused.
///
/// Each arm names the **button the operator pressed**, not the wire's
/// spelling, so the sentence is about the thing they just did.
#[must_use]
pub fn refusal(request: &hytte_plugin_agents::hive::wire::Request, reason: &str) -> String {
    use hytte_plugin_agents::hive::wire::Request;
    match request {
        Request::Start { .. } => format!("couldn't start this agent: {reason}"),
        Request::Stop { .. } => format!("couldn't stop this agent: {reason}"),
        Request::SetPaused { paused: true, .. } => {
            format!("couldn't pause this agent: {reason}")
        }
        Request::SetPaused { paused: false, .. } => {
            format!("couldn't resume this agent: {reason}")
        }
        // Unreachable for the three verbs this window sends — but it is a
        // sentence, so it parses. The old spelling interpolated a verb phrase
        // into a slot shaped for a bare verb and read "couldn't ask the hive
        // to this agent" (#1130 L6).
        _ => format!("the hive refused that: {reason}"),
    }
}

#[cfg(test)]
mod tests {
    use super::refusal;
    use hytte_plugin_agents::hive::wire::{Request, Scope};

    /// Every arm is a sentence that parses, including the one no verb this
    /// window sends can reach.
    ///
    /// Mutation (re-run this round, red): restore the old `_ => "ask the hive
    /// to"` arm, which produced "couldn't ask the hive to this agent" — the
    /// grammar half of #1130 L6.
    #[test]
    fn every_refusal_arm_is_a_sentence() {
        use hytte_plugin_agents::hive::wire::Request;
        let all = [
            Request::Start {
                scope: Scope::agent("stray"),
            },
            Request::Stop {
                scope: Scope::agent("stray"),
                graceful: true,
            },
            Request::SetPaused {
                name: "stray".to_owned(),
                paused: true,
            },
            Request::SetPaused {
                name: "stray".to_owned(),
                paused: false,
            },
            // The `_` arm.
            Request::AgentStatus,
        ];
        for req in all {
            let s = refusal(&req, "the reason");
            assert!(s.ends_with("the reason"), "{s}");
            assert!(
                !s.contains("to this agent"),
                "a verb phrase in a bare-verb slot does not parse: {s}"
            );
            assert!(
                s.starts_with("couldn't ") || s.starts_with("the hive "),
                "{s}"
            );
        }
    }

    /// A refusal names the verb the operator pressed, not the wire's spelling.
    #[test]
    fn a_refusal_names_the_button_that_was_pressed() {
        assert!(
            refusal(
                &Request::Start {
                    scope: Scope::agent("stray")
                },
                "no such agent"
            )
            .starts_with("couldn't start this agent")
        );
        assert!(
            refusal(
                &Request::SetPaused {
                    name: "stray".to_owned(),
                    paused: false
                },
                "busy"
            )
            .starts_with("couldn't resume this agent")
        );
        assert!(
            refusal(
                &Request::SetPaused {
                    name: "stray".to_owned(),
                    paused: true
                },
                "busy"
            )
            .starts_with("couldn't pause this agent")
        );
    }
}

#[cfg(all(test, feature = "system-tests"))]
mod gtk_tests {
    use super::{Header, Press, Settings};
    use crate::chrome::{Controls, Facts, HeaderModel};
    use crate::feed::AgentState;
    use gtk::prelude::{ToggleButtonExt as _, WidgetExt as _};
    use hytte_plugin_agents::config::AgentsConfig;
    use hytte_plugin_agents::hive::wire::AgentStatusRow;
    use hytte_plugin_agents::model::{Agent, AgentName};
    use std::cell::RefCell;
    use std::rc::Rc;

    fn name(s: &str) -> AgentName {
        AgentName::parse(s).expect("a legal test name")
    }

    fn up(row: AgentStatusRow) -> AgentState {
        AgentState::Up(Box::new(Agent {
            name: name(&row.name.clone()),
            row,
            pending_paused: None,
        }))
    }

    fn show(header: &Header, state: &AgentState) {
        header.apply(
            &HeaderModel::of(&name("stray"), &AgentsConfig::default(), state),
            &Controls::of(state),
        );
    }

    /// **The header follows the hive.** Two states, applied in turn, and the
    /// label on screen changes with them — including the buttons, which swap
    /// rather than both showing.
    ///
    /// This is the widget half of "a status change updates the header"; the
    /// socket half is `tests/feed.rs`. Falsification: drop the
    /// `set_text` in `Header::apply` and the second assertion reds while the
    /// pure `chrome` tests stay green — which is exactly the gap this covers.
    #[gtk::test]
    fn applying_a_new_state_rewrites_the_header() {
        let header = Header::new();

        show(&header, &AgentState::Connecting);
        assert_eq!(header.status_text(), crate::chrome::CONNECTING);
        assert_eq!(header.model_text(), None, "no agent, no model chip");

        show(
            &header,
            &up(AgentStatusRow {
                name: "stray".to_owned(),
                running: true,
                active_model: Some("claude-opus-5-20262981".to_owned()),
                status_text: Some("reviewing PR #963".to_owned()),
                ..AgentStatusRow::default()
            }),
        );
        assert_eq!(header.title_text(), "stray");
        assert_eq!(header.status_text(), "reviewing PR #963");
        assert_eq!(header.model_text().as_deref(), Some("Opus"));

        show(
            &header,
            &up(AgentStatusRow {
                name: "stray".to_owned(),
                failed: true,
                ..AgentStatusRow::default()
            }),
        );
        assert_eq!(header.status_text(), "failed");
    }

    /// **`Controls` reaches the buttons.** For every `Status`, what
    /// `Header::apply` leaves on screen is what `Controls::of` decided.
    ///
    /// `chrome::tests` pins the decision; this pins its *application*, which
    /// nothing covered — all five `set_visible`/`set_sensitive` lines in
    /// `apply` could be deleted with the suite green (#1130 N2). The gap was
    /// widened by `press_*_for_test` using `emit_clicked`, which fires
    /// regardless of sensitivity; those helpers now refuse an insensitive
    /// button, so the two halves cannot drift apart again.
    ///
    /// Mutation (re-run this round, red): delete any of the five lines and
    /// this reds on the row it governs — measured for all five.
    #[gtk::test]
    fn the_controls_reach_the_buttons() {
        let header = Header::new();

        // Before the hive answers: nothing is pressable, and Start is the one
        // that shows (a dead Stop beside a dead Start reads as two broken
        // buttons rather than one unknown state).
        show(&header, &AgentState::Connecting);
        assert_eq!(
            header.button_states(),
            [(true, false), (false, false), (true, false)],
            "connecting: Start visible-but-dead, Stop hidden, pause dead"
        );

        let row = |f: fn(&mut AgentStatusRow)| {
            let mut r = AgentStatusRow {
                name: "stray".to_owned(),
                running: true,
                ..AgentStatusRow::default()
            };
            f(&mut r);
            up(r)
        };

        // Running: Stop is offered, Start is not, pause is live and up.
        show(&header, &row(|_| {}));
        assert_eq!(
            header.button_states(),
            [(false, false), (true, true), (true, true)],
            "running: Stop is the offer"
        );
        assert!(!header.pause_is_active());

        // Paused: still running, so still Stop — and the toggle is down.
        show(&header, &row(|r| r.paused = true));
        assert_eq!(
            header.button_states(),
            [(false, false), (true, true), (true, true)],
            "paused is a running agent: it is offered Stop, not Start"
        );
        assert!(header.pause_is_active());

        // Stopped: the offer swaps.
        show(&header, &row(|r| r.running = false));
        assert_eq!(
            header.button_states(),
            [(true, true), (false, false), (true, true)],
            "stopped: Start is the offer"
        );
        assert!(!header.pause_is_active());

        // Failed and NeedsLogin are not running, but they are not `Stopped`
        // either — P1's rule, and the two rows #1130's M15 slipped through.
        show(
            &header,
            &row(|r| {
                r.running = false;
                r.failed = true;
            }),
        );
        assert_eq!(
            header.button_states(),
            [(false, false), (true, true), (true, true)],
            "failed: offered Stop, matching the card"
        );
        show(
            &header,
            &row(|r| {
                r.running = false;
                r.needs_login = true;
            }),
        );
        assert_eq!(
            header.button_states(),
            [(false, false), (true, true), (true, true)],
            "needs_login: offered Stop, matching the card"
        );
    }

    /// A **programmatic** pause flip does not read back as a click.
    ///
    /// The hive is the source of truth for `paused`, so every poll re-applies
    /// it; without the echo guard each one would fire `toggled` and send
    /// another `SetPaused`, which is a feedback loop against a daemon.
    ///
    /// Mutation (verified red, #1130 review M14): delete the `echo` guard in `Header::apply` and
    /// the first assertion reds.
    #[gtk::test]
    fn a_polled_pause_state_does_not_send_a_command() {
        let header = Header::new();
        let presses: Rc<RefCell<Vec<Press>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = Rc::clone(&presses);
        header.connect(move |p| sink.borrow_mut().push(p));

        let paused = up(AgentStatusRow {
            name: "stray".to_owned(),
            running: true,
            paused: true,
            ..AgentStatusRow::default()
        });
        show(&header, &paused);
        show(&header, &paused);
        assert!(
            presses.borrow().is_empty(),
            "the hive telling us it is paused is not the operator asking for it: {:?}",
            presses.borrow()
        );

        // …and a toggle that did **not** come from `apply` still reaches the
        // handler, so the guard narrows nothing it should not.
        header.pause.set_active(false);
        assert_eq!(presses.borrow().as_slice(), [Press::SetPaused(false)]);
    }

    /// **A rebuild takes the old rows out of the group**, not just out of our
    /// `Vec`. The reviewer's test (#1130 M4), taken as supplied.
    ///
    /// `Settings::apply` runs on every state change, so a rebuild that only
    /// drained the bookkeeping would leak seven `ActionRow`s into the
    /// `PreferencesGroup` per poll — and the sibling test below could not see
    /// it, because it reads back from the same vectors the drain empties.
    ///
    /// Mutation (re-run this round, red): the reviewer's **M12** — replace the
    /// removal loop with `tracked.borrow_mut().clear();`, keeping the
    /// bookkeeping perfect — and this reds while the sibling stays green,
    /// which is exactly the gap it was written for.
    #[gtk::test]
    fn a_rebuild_takes_the_old_rows_out_of_the_group() {
        let settings = Settings::new();
        let cfg = AgentsConfig::default();

        settings.apply(
            &Facts::agent(&name("stray"), &AgentState::Connecting),
            &Facts::hive(&cfg, None),
        );
        let first = settings.tracked_rows();
        assert!(
            !first.is_empty() && first.iter().all(|r| r.parent().is_some()),
            "the first apply must actually put the rows in the group"
        );

        let state = up(AgentStatusRow {
            name: "stray".to_owned(),
            running: true,
            ..AgentStatusRow::default()
        });
        settings.apply(
            &Facts::agent(&name("stray"), &state),
            &Facts::hive(&cfg, None),
        );
        assert!(
            first.iter().all(|r| r.parent().is_none()),
            "a rebuilt group must drop its old rows from the widget tree, not just from our Vec"
        );
        assert!(
            settings.tracked_rows().iter().all(|r| r.parent().is_some()),
            "…and the new ones must be in it"
        );
    }

    /// The settings page shows every fact, and the tracked count does not
    /// grow — the bookkeeping half, kept beside the widget-tree half above so
    /// a reader can see which is which.
    #[gtk::test]
    fn the_settings_page_rebuilds_without_stale_rows() {
        let settings = Settings::new();
        let cfg = AgentsConfig::default();
        let facts =
            |state: &AgentState| (Facts::agent(&name("stray"), state), Facts::hive(&cfg, None));

        let (a, h) = facts(&AgentState::Connecting);
        let expected = a.len() + h.len();
        settings.apply(&a, &h);
        assert_eq!(settings.row_text().len(), expected);

        let state = up(AgentStatusRow {
            name: "stray".to_owned(),
            running: true,
            active_model: Some("claude-opus-5-20262981".to_owned()),
            ..AgentStatusRow::default()
        });
        let (a, h) = facts(&state);
        settings.apply(&a, &h);
        assert_eq!(settings.row_text().len(), expected, "rebuilt, not appended");
        assert!(
            settings
                .row_text()
                .iter()
                .any(|r| r == "Model: claude-opus-5-20262981"),
            "{:?}",
            settings.row_text()
        );
    }
}
