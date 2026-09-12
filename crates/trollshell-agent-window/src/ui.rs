//! The widgets: the header line, the lifecycle buttons, and the settings page.
//!
//! Everything here is a `set_*` call driven by [`crate::chrome`]'s models, so
//! the rules live where a hermetic test can reach them and this layer is what
//! one display test pins.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;

use crate::chrome::{ApprovalRow, Controls, Fact, HeaderModel};

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

/// The settings tab: what the hive says about this agent (read-only), and
/// since #1141 the one thing on this page that writes — the agent's queued
/// approvals.
pub struct Settings {
    /// The widget to mount in the `ViewStack`.
    pub root: adw::PreferencesPage,
    approvals: Approvals,
    agent: adw::PreferencesGroup,
    hive: adw::PreferencesGroup,
    agent_rows: RefCell<Vec<adw::ActionRow>>,
    hive_rows: RefCell<Vec<adw::ActionRow>>,
}

impl Settings {
    /// Build the empty page.
    #[must_use]
    pub fn new() -> Self {
        let approvals = Approvals::new();
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
        // Approvals first — it is the one group asking for a decision, ahead
        // of the two that only report.
        root.add(&approvals.root);
        root.add(&agent);
        root.add(&hive);
        Self {
            root,
            approvals,
            agent,
            hive,
            agent_rows: RefCell::new(Vec::new()),
            hive_rows: RefCell::new(Vec::new()),
        }
    }

    /// Route every Approve/Deny press to `on_decision` (#1141).
    pub fn connect_decision(&self, on_decision: impl Fn(Decision) + 'static) {
        self.approvals.connect(on_decision);
    }

    /// Replace every group's rows: the two read-only ones plus the
    /// approvals group.
    ///
    /// Rebuilt rather than updated in place: the row set is short and fixed,
    /// and a rebuild cannot leave a stale row behind when a field goes from
    /// present to absent.
    ///
    /// Rows are `adw::ActionRow`s — `GtkListBoxRow`s — because a
    /// `PreferencesGroup` renders anything else *below* its list rather than
    /// among the rows, which type-checks and looks wrong.
    pub fn apply(
        &self,
        agent: &[Fact],
        hive: &[Fact],
        approvals: &[ApprovalRow],
        approvals_refused: Option<&str>,
    ) {
        self.approvals.apply(approvals, approvals_refused);
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

    /// The approvals group's own rows, `title: subtitle` — kept separate from
    /// [`Settings::row_text`] because a subtitle here can legitimately
    /// contain the same text twice in two different tests' expectations
    /// (a refusal reason), and mixing the two groups would make a length
    /// assertion ambiguous about which group grew.
    #[cfg(all(test, feature = "system-tests"))]
    pub fn approval_row_text(&self) -> Vec<String> {
        self.approvals.row_text()
    }

    /// Whether the Approvals group is showing at all — hidden when the queue
    /// is empty.
    #[cfg(all(test, feature = "system-tests"))]
    pub fn approvals_visible(&self) -> bool {
        self.approvals.root.property::<bool>("visible")
    }

    /// The approvals group's rows as it **tracks** them — see
    /// [`Settings::tracked_rows`] for why a test asks the container rather
    /// than the bookkeeping, and `approval_rows_are_rebuilt_not_appended` for
    /// the hole that reopened here (#1146's review, M2).
    #[cfg(all(test, feature = "system-tests"))]
    pub fn tracked_approval_rows(&self) -> Vec<adw::ActionRow> {
        self.approvals.tracked_rows()
    }

    /// Press Approve for `id` in the approvals group. See
    /// `Header::press_start_for_test`.
    #[cfg(all(test, feature = "system-tests"))]
    pub fn press_approve_for_test(&self, id: i64) {
        self.approvals.press_approve_for_test(id);
    }

    /// Press Deny for `id`. See [`Settings::press_approve_for_test`].
    #[cfg(all(test, feature = "system-tests"))]
    pub fn press_deny_for_test(&self, id: i64) {
        self.approvals.press_deny_for_test(id);
    }

    /// Try to press Approve for `id`, answering **whether the operator could
    /// have** — `false` for a button that is there but insensitive, which is
    /// what an in-flight row's buttons are (#1146's review, H1).
    ///
    /// The sibling above asserts pressability, which is right for a test
    /// about a live row and wrong for one about a latched one: a second click
    /// on a latched row is not a panic, it is a click GTK swallows.
    #[cfg(all(test, feature = "system-tests"))]
    pub fn try_press_approve_for_test(&self, id: i64) -> bool {
        self.approvals.try_press_for_test(id, Decision::Approve(id))
    }

    /// Try to press Deny for `id`. See
    /// [`Settings::try_press_approve_for_test`].
    #[cfg(all(test, feature = "system-tests"))]
    pub fn try_press_deny_for_test(&self, id: i64) -> bool {
        self.approvals.try_press_for_test(id, Decision::Deny(id))
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self::new()
    }
}

/// One decision a row's Approve/Deny button asks for (#1141, spec §6.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Approve the approval with this id.
    Approve(i64),
    /// Deny it.
    Deny(i64),
}

impl Decision {
    /// The id it names, regardless of which button raised it — what the
    /// window's staleness guard checks before either becomes a frame.
    #[must_use]
    pub fn id(self) -> i64 {
        match self {
            Self::Approve(id) | Self::Deny(id) => id,
        }
    }
}

/// One approval row's widgets — an `ActionRow` plus the two buttons in its
/// suffix, tracked together so a test can find "the Approve button for id
/// 42" without walking the widget tree.
#[allow(
    dead_code,
    reason = "id, approve and deny are read back only by the system-tests press helpers below; \
              a plain build never needs them once the buttons are wired and parented"
)]
struct ApprovalRowWidgets {
    id: i64,
    row: adw::ActionRow,
    approve: gtk::Button,
    deny: gtk::Button,
}

/// The callback slot a row's Approve/Deny buttons close over — named so
/// `Approvals`'s field escapes `clippy::type_complexity`.
type DecisionHandler = Rc<dyn Fn(Decision)>;

/// What the approvals group says when the hive refused to hand over the queue
/// at all (#1146's review, M1) — a state of its own, because the rows are
/// cleared for this the same way they are cleared for an empty queue.
pub const APPROVALS_REFUSED: &str = "The hive refused the approval queue";

/// The Settings page's "Approvals" group: the agent's queued decisions,
/// Approve/Deny per row (#1141, spec §6.5).
///
/// Hidden whenever the queue is empty, which is most of the time: an operator
/// who has nothing to approve should not carry a permanently-empty group on
/// a page that otherwise only reports facts.
pub struct Approvals {
    /// The widget to mount in the settings page.
    pub root: adw::PreferencesGroup,
    rows: RefCell<Vec<ApprovalRowWidgets>>,
    /// The one row shown instead of the queue when the hive refused to hand
    /// it over (#1146's review, M1). Tracked separately from `rows` because
    /// it carries no id and no buttons — nothing can be decided about a queue
    /// nobody can read.
    refusal: RefCell<Option<adw::ActionRow>>,
    on_decision: RefCell<Option<DecisionHandler>>,
}

impl Approvals {
    /// Build the empty group.
    #[must_use]
    pub fn new() -> Self {
        let root = adw::PreferencesGroup::builder()
            .title("Approvals")
            .description(
                "Queued decisions this agent is waiting on. Approve and Deny act \
                 immediately; an unanswered row stays exactly as it is — silence never \
                 decides it.",
            )
            .build();
        root.set_visible(false);
        Self {
            root,
            rows: RefCell::new(Vec::new()),
            refusal: RefCell::new(None),
            on_decision: RefCell::new(None),
        }
    }

    /// Route every Approve/Deny press to `on_decision`.
    pub fn connect(&self, on_decision: impl Fn(Decision) + 'static) {
        *self.on_decision.borrow_mut() = Some(Rc::new(on_decision));
    }

    /// Replace the rows. Rebuilt rather than updated in place, exactly like
    /// [`Settings::apply`]'s two groups: the row set is short, and a decided
    /// approval must not linger as a stale row with live buttons on a queue
    /// it has already left.
    pub fn apply(&self, approvals: &[ApprovalRow], refused: Option<&str>) {
        for w in self.rows.borrow_mut().drain(..) {
            self.root.remove(&w.row);
        }
        if let Some(row) = self.refusal.borrow_mut().take() {
            self.root.remove(&row);
        }
        self.root
            .set_visible(!approvals.is_empty() || refused.is_some());

        // A hive that will not answer the queue is **not** an empty queue
        // (#1146's review, M1): the rows are gone either way, so without a
        // state of its own an older daemon or a permissions change rendered
        // exactly like "nothing to decide".
        if let Some(reason) = refused {
            let row = adw::ActionRow::builder()
                .title(APPROVALS_REFUSED)
                .subtitle(reason)
                .use_markup(false)
                .subtitle_selectable(true)
                .build();
            row.set_subtitle_lines(3);
            row.add_css_class("warning");
            self.root.add(&row);
            *self.refusal.borrow_mut() = Some(row);
        }

        let mut tracked = self.rows.borrow_mut();
        for a in approvals {
            let approve = gtk::Button::builder().label("Approve").build();
            approve.set_valign(gtk::Align::Center);
            approve.add_css_class("suggested-action");
            let deny = gtk::Button::builder().label("Deny").build();
            deny.set_valign(gtk::Align::Center);
            deny.add_css_class("destructive-action");

            if let Some(cb) = self.on_decision.borrow().clone() {
                let id = a.id;
                let f = Rc::clone(&cb);
                approve.connect_clicked(move |_| f(Decision::Approve(id)));
                let f = Rc::clone(&cb);
                deny.connect_clicked(move |_| f(Decision::Deny(id)));
            }

            // **The latch** (#1146's review, H1). A decision already on the
            // wire leaves the row exactly where it is until the next poll, so
            // without this its two buttons stay live for a whole cadence —
            // long enough for a double-click to send the same frame twice, or
            // for an operator who saw no feedback to send `Deny` behind an
            // `Approve` that already succeeded.
            approve.set_sensitive(!a.in_flight);
            deny.set_sensitive(!a.in_flight);

            let suffix = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            suffix.set_valign(gtk::Align::Center);
            suffix.append(&approve);
            suffix.append(&deny);

            let row = adw::ActionRow::builder()
                .title(&a.title)
                .subtitle(a.subtitle())
                .use_markup(false)
                .subtitle_selectable(true)
                .build();
            // Room for the detail line, the stamp and (when a decision on
            // this row was refused) the reason on its own line.
            row.set_subtitle_lines(3);
            row.add_suffix(&suffix);
            self.root.add(&row);
            tracked.push(ApprovalRowWidgets {
                id: a.id,
                row,
                approve,
                deny,
            });
        }
    }

    /// Every row's `title: subtitle` — see [`Settings::row_text`]'s doc for
    /// why this group's rows are read back separately. The refusal row is
    /// included, because it is what the group shows *instead of* rows.
    #[cfg(all(test, feature = "system-tests"))]
    fn row_text(&self) -> Vec<String> {
        let text =
            |r: &adw::ActionRow| format!("{}: {}", r.title(), r.subtitle().unwrap_or_default());
        self.refusal
            .borrow()
            .iter()
            .map(text)
            .chain(self.rows.borrow().iter().map(|w| text(&w.row)))
            .collect()
    }

    /// The rows this group **tracks**, for the tests that ask the container
    /// what became of them (#1146's review, M2). Chains the refusal row too
    /// (#1146 re-verify, L-NEW-1) — `row_text` already reads it back, and a
    /// tracker that only mirrored `rows` was blind to a mutation that drops
    /// the refusal row from its own bookkeeping without removing it from the
    /// container. See [`Settings::tracked_rows`] for the argument.
    #[cfg(all(test, feature = "system-tests"))]
    fn tracked_rows(&self) -> Vec<adw::ActionRow> {
        self.refusal
            .borrow()
            .iter()
            .cloned()
            .chain(self.rows.borrow().iter().map(|w| w.row.clone()))
            .collect()
    }

    /// The button `which` names for `id`, **cloned out of the borrow**.
    ///
    /// Cloning matters: a click handler reaches `Window::on_decision`, which
    /// since #1146's review repaints so the answered row stops looking
    /// answerable — and that rebuild takes `rows` mutably. A helper that held
    /// the borrow across `emit_clicked` would panic `RefCell already
    /// borrowed` on a re-entrancy a real click never has (GTK delivers it
    /// with nothing of ours on the stack). `gtk::Button` is a refcounted
    /// handle, so the clone is the same widget.
    #[cfg(all(test, feature = "system-tests"))]
    fn button_for_test(&self, id: i64, which: Decision) -> gtk::Button {
        let rows = self.rows.borrow();
        let w = rows
            .iter()
            .find(|w| w.id == id)
            .unwrap_or_else(|| panic!("no approval row for id {id}"));
        match which {
            Decision::Approve(_) => w.approve.clone(),
            Decision::Deny(_) => w.deny.clone(),
        }
    }

    /// Press one of `id`'s two buttons **if it is pressable**, answering
    /// whether it was — the latch's read-back (#1146's review, H1).
    #[cfg(all(test, feature = "system-tests"))]
    fn try_press_for_test(&self, id: i64, which: Decision) -> bool {
        let button = self.button_for_test(id, which);
        let (visible, sensitive) = own_flags(&button);
        if !(visible && sensitive) {
            return false;
        }
        button.emit_clicked();
        true
    }

    /// Press Approve for `id`, refusing an insensitive or absent button —
    /// what an operator could actually do. See
    /// `Header::press_start_for_test`.
    #[cfg(all(test, feature = "system-tests"))]
    fn press_approve_for_test(&self, id: i64) {
        assert!(
            self.try_press_for_test(id, Decision::Approve(id)),
            "Approve for {id} is not pressable"
        );
    }

    /// Press Deny for `id`. See [`Approvals::press_approve_for_test`].
    #[cfg(all(test, feature = "system-tests"))]
    fn press_deny_for_test(&self, id: i64) {
        assert!(
            self.try_press_for_test(id, Decision::Deny(id)),
            "Deny for {id} is not pressable"
        );
    }
}

impl Default for Approvals {
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
            &[],
            None,
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
            &[],
            None,
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
        settings.apply(&a, &h, &[], None);
        assert_eq!(settings.row_text().len(), expected);

        let state = up(AgentStatusRow {
            name: "stray".to_owned(),
            running: true,
            active_model: Some("claude-opus-5-20262981".to_owned()),
            ..AgentStatusRow::default()
        });
        let (a, h) = facts(&state);
        settings.apply(&a, &h, &[], None);
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
