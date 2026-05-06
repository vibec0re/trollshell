//! Audio drawer panel — output sinks, input sources, and per-stream
//! playback volume.
//!
//! Each section is a `boxed-list`-styled `gtk::ListBox` that diffs every
//! pipewire snapshot against a live per-row map and updates fields in
//! place. Rebuilding rows on each emission would tear down the slider
//! the user is holding, so rows are kept alive across emissions and the
//! snapshot drives field-level updates.
//!
//! Echo cancellation: pipewire's polling is async, so a snapshot that
//! arrives after a `pactl` write may still report the OLD volume. Each
//! row tracks a `pending` value the user just sent; while pending,
//! mismatched snapshots are ignored to keep the slider thumb where the
//! user left it. Pending clears once pipewire confirms (snapshot ≈
//! pending) or after `ECHO_TIMEOUT` as a safety net.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, Instant};

use hytte::gtk::{self, glib, prelude::*};
use hytte::prelude::*;
use hytte::services::pipewire::{self, PlaybackStream, Sink, Source};

use crate::components::layout::{finish_page, page_box};

/// Snapshot considered to match our pending write when within this much
/// of the value we sent. `pactl` rounds to integer percent so 0.005 is a
/// comfortable margin below the 0.01 step.
const ECHO_TOLERANCE: f64 = 0.005;

/// Stop ignoring snapshots after this long, in case `pactl` failed and
/// no confirming snapshot will ever arrive.
const ECHO_TIMEOUT: Duration = Duration::from_secs(1);

/// Tolerance for skipping a redundant programmatic `set_value` when the
/// slider already shows the same volume as the snapshot.
const SLIDER_NOOP_TOLERANCE: f64 = 0.001;

pub fn panel_audio() -> gtk::Widget {
    let column = page_box();
    column.append(&audio_section("Output", &build_sink_list()));
    column.append(&audio_section("Input", &build_source_list()));
    column.append(&audio_section("Playback", &build_playback_list()));
    finish_page(&column)
}

fn audio_section(title: &str, list: &gtk::ListBox) -> gtk::Box {
    let section = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let title_lbl = gtk::Label::new(Some(title));
    title_lbl.add_css_class("heading");
    title_lbl.set_xalign(0.0);
    section.append(&title_lbl);
    section.append(list);
    section
}

fn boxed_list() -> gtk::ListBox {
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    list
}

fn truncate_desc(s: &str) -> String {
    if s.len() > 40 {
        format!("{}…", &s[..39])
    } else {
        s.to_string()
    }
}

fn default_radio_glyph(is_default: bool) -> &'static str {
    if is_default { "\u{25cf}" } else { "\u{25cb}" }
}

fn toggle_class<W: IsA<gtk::Widget>>(widget: &W, class: &str, on: bool) {
    if on {
        widget.add_css_class(class);
    } else {
        widget.remove_css_class(class);
    }
}

/// Should the snapshot value be applied to the UI? Returns `true` when
/// no write is pending, the snapshot confirms the pending write, or the
/// pending write has timed out. Mutates `pending` to clear on
/// confirmation/timeout.
fn echo_settled<T: Copy + PartialEq>(
    pending: &Cell<Option<(T, Instant)>>,
    snapshot: T,
    matches: impl FnOnce(T, T) -> bool,
) -> bool {
    match pending.get() {
        None => true,
        Some((expected, sent)) => {
            if matches(expected, snapshot) || sent.elapsed() > ECHO_TIMEOUT {
                pending.set(None);
                true
            } else {
                false
            }
        }
    }
}

// ── Sinks ────────────────────────────────────────────────────────────────────

struct SinkRow {
    /// `ListBox::append` auto-wraps any non-`ListBoxRow` child in a
    /// generated `ListBoxRow` and uses *that* as its child — so passing
    /// the inner widget back to `ListBox::remove` later silently fails
    /// ("Tried to remove non-child"). We wrap explicitly so the same
    /// handle is used for both append and remove.
    row: gtk::ListBoxRow,
    widget: gtk::Box,
    radio_btn: gtk::Button,
    radio_lbl: gtk::Label,
    name_lbl: gtk::Label,
    slider: gtk::Scale,
    /// Volume the user just sent + when. Cleared once pipewire confirms.
    pending_volume: Rc<Cell<Option<(f64, Instant)>>>,
    mute_btn: gtk::Button,
    muted_cell: Rc<Cell<bool>>,
    pending_mute: Rc<Cell<Option<(bool, Instant)>>>,
    cached_desc: RefCell<String>,
}

impl SinkRow {
    fn new(s: &Sink) -> Self {
        let widget = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        widget.add_css_class("ts-audio-row");

        let radio_lbl = gtk::Label::new(Some(default_radio_glyph(s.is_default)));
        let radio_btn = gtk::Button::new();
        radio_btn.set_child(Some(&radio_lbl));
        radio_btn.add_css_class("ts-audio-default-btn");
        let name_for_radio = s.name.clone();
        radio_btn.connect_clicked(move |_| pipewire::set_default_sink(&name_for_radio));
        widget.append(&radio_btn);

        let initial_desc = truncate_desc(&s.description);
        let name_lbl = gtk::Label::new(Some(&initial_desc));
        name_lbl.set_xalign(0.0);
        name_lbl.set_hexpand(true);
        name_lbl.add_css_class("ts-audio-row-name");
        widget.append(&name_lbl);

        let slider = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.05);
        slider.set_draw_value(false);
        slider.set_hexpand(true);
        slider.set_size_request(110, -1);
        slider.set_value(s.volume);

        let pending_volume: Rc<Cell<Option<(f64, Instant)>>> = Rc::new(Cell::new(None));
        let pending_for_handler = pending_volume.clone();
        let name_for_slider = s.name.clone();
        // `change-value` only fires for user input (drag, scroll, keys);
        // programmatic `set_value` does NOT trigger it, so the
        // snapshot-driven update path can't echo back to pactl.
        slider.connect_change_value(move |_, _, val| {
            pending_for_handler.set(Some((val, Instant::now())));
            pipewire::set_sink_volume(&name_for_slider, val);
            glib::Propagation::Proceed
        });
        widget.append(&slider);

        let mute_btn = gtk::Button::from_icon_name("audio-volume-muted-symbolic");
        mute_btn.add_css_class("ts-audio-mute-btn");
        let muted_cell = Rc::new(Cell::new(s.muted));
        let pending_mute: Rc<Cell<Option<(bool, Instant)>>> = Rc::new(Cell::new(None));
        let muted_for_click = muted_cell.clone();
        let pending_for_click = pending_mute.clone();
        let name_for_mute = s.name.clone();
        mute_btn.connect_clicked(move |btn| {
            let new_mute = !muted_for_click.get();
            muted_for_click.set(new_mute);
            pending_for_click.set(Some((new_mute, Instant::now())));
            pipewire::set_sink_mute(&name_for_mute, new_mute);
            toggle_class(btn, "muted", new_mute);
        });
        widget.append(&mute_btn);

        let lbr = gtk::ListBoxRow::new();
        lbr.set_child(Some(&widget));
        let row = SinkRow {
            row: lbr,
            widget,
            radio_btn,
            radio_lbl,
            name_lbl,
            slider,
            pending_volume,
            mute_btn,
            muted_cell,
            pending_mute,
            cached_desc: RefCell::new(initial_desc),
        };
        row.apply_meta(s);
        row
    }

    fn apply_meta(&self, s: &Sink) {
        toggle_class(&self.widget, "default", s.is_default);
        toggle_class(&self.radio_btn, "active", s.is_default);
        self.radio_lbl.set_text(default_radio_glyph(s.is_default));
        toggle_class(&self.name_lbl, "dim", !s.is_default);
        let desc = truncate_desc(&s.description);
        if *self.cached_desc.borrow() != desc {
            self.name_lbl.set_text(&desc);
            if s.description.len() > 40 {
                self.name_lbl.set_tooltip_text(Some(&s.description));
            } else {
                self.name_lbl.set_tooltip_text(None);
            }
            *self.cached_desc.borrow_mut() = desc;
        }
    }

    fn update(&self, s: &Sink) {
        self.apply_meta(s);

        if echo_settled(&self.pending_mute, s.muted, |a, b| a == b) {
            self.muted_cell.set(s.muted);
            toggle_class(&self.mute_btn, "muted", s.muted);
        }

        if echo_settled(&self.pending_volume, s.volume, |a, b| (a - b).abs() < ECHO_TOLERANCE)
            && (self.slider.value() - s.volume).abs() > SLIDER_NOOP_TOLERANCE
        {
            self.slider.set_value(s.volume);
        }
    }
}

fn build_sink_list() -> gtk::ListBox {
    let list = boxed_list();
    let rows: Rc<RefCell<HashMap<String, SinkRow>>> = Rc::new(RefCell::new(HashMap::new()));
    let list_for_bind = list.clone();
    let rows_for_bind = rows.clone();
    bind(pipewire::sinks(), &list, move |_, sinks: Vec<Sink>| {
        let mut rows = rows_for_bind.borrow_mut();
        let new_keys: HashSet<&str> = sinks.iter().map(|s| s.name.as_str()).collect();
        let gone: Vec<String> = rows
            .keys()
            .filter(|k| !new_keys.contains(k.as_str()))
            .cloned()
            .collect();
        for k in gone {
            if let Some(r) = rows.remove(&k) {
                list_for_bind.remove(&r.row);
            }
        }
        for s in &sinks {
            if let Some(row) = rows.get(&s.name) {
                row.update(s);
            } else {
                let row = SinkRow::new(s);
                list_for_bind.append(&row.row);
                rows.insert(s.name.clone(), row);
            }
        }
    });
    list
}

// ── Sources ──────────────────────────────────────────────────────────────────

struct SourceRow {
    row: gtk::ListBoxRow,
    widget: gtk::Box,
    radio_btn: gtk::Button,
    radio_lbl: gtk::Label,
    name_lbl: gtk::Label,
    slider: gtk::Scale,
    pending_volume: Rc<Cell<Option<(f64, Instant)>>>,
    mute_btn: gtk::Button,
    muted_cell: Rc<Cell<bool>>,
    pending_mute: Rc<Cell<Option<(bool, Instant)>>>,
    cached_desc: RefCell<String>,
}

impl SourceRow {
    fn new(s: &Source) -> Self {
        let widget = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        widget.add_css_class("ts-audio-row");

        let radio_lbl = gtk::Label::new(Some(default_radio_glyph(s.is_default)));
        let radio_btn = gtk::Button::new();
        radio_btn.set_child(Some(&radio_lbl));
        radio_btn.add_css_class("ts-audio-default-btn");
        let name_for_radio = s.name.clone();
        radio_btn.connect_clicked(move |_| pipewire::set_default_source(&name_for_radio));
        widget.append(&radio_btn);

        let initial_desc = truncate_desc(&s.description);
        let name_lbl = gtk::Label::new(Some(&initial_desc));
        name_lbl.set_xalign(0.0);
        name_lbl.set_hexpand(true);
        name_lbl.add_css_class("ts-audio-row-name");
        widget.append(&name_lbl);

        let slider = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.05);
        slider.set_draw_value(false);
        slider.set_hexpand(true);
        slider.set_size_request(110, -1);
        slider.set_value(s.volume);

        let pending_volume: Rc<Cell<Option<(f64, Instant)>>> = Rc::new(Cell::new(None));
        let pending_for_handler = pending_volume.clone();
        let name_for_slider = s.name.clone();
        slider.connect_change_value(move |_, _, val| {
            pending_for_handler.set(Some((val, Instant::now())));
            pipewire::set_source_volume(&name_for_slider, val);
            glib::Propagation::Proceed
        });
        widget.append(&slider);

        let mute_btn = gtk::Button::from_icon_name("audio-volume-muted-symbolic");
        mute_btn.add_css_class("ts-audio-mute-btn");
        let muted_cell = Rc::new(Cell::new(s.muted));
        let pending_mute: Rc<Cell<Option<(bool, Instant)>>> = Rc::new(Cell::new(None));
        let muted_for_click = muted_cell.clone();
        let pending_for_click = pending_mute.clone();
        let name_for_mute = s.name.clone();
        mute_btn.connect_clicked(move |btn| {
            let new_mute = !muted_for_click.get();
            muted_for_click.set(new_mute);
            pending_for_click.set(Some((new_mute, Instant::now())));
            pipewire::set_source_mute(&name_for_mute, new_mute);
            toggle_class(btn, "muted", new_mute);
        });
        widget.append(&mute_btn);

        let lbr = gtk::ListBoxRow::new();
        lbr.set_child(Some(&widget));
        let row = SourceRow {
            row: lbr,
            widget,
            radio_btn,
            radio_lbl,
            name_lbl,
            slider,
            pending_volume,
            mute_btn,
            muted_cell,
            pending_mute,
            cached_desc: RefCell::new(initial_desc),
        };
        row.apply_meta(s);
        row
    }

    fn apply_meta(&self, s: &Source) {
        toggle_class(&self.widget, "default", s.is_default);
        toggle_class(&self.radio_btn, "active", s.is_default);
        self.radio_lbl.set_text(default_radio_glyph(s.is_default));
        toggle_class(&self.name_lbl, "dim", !s.is_default);
        let desc = truncate_desc(&s.description);
        if *self.cached_desc.borrow() != desc {
            self.name_lbl.set_text(&desc);
            if s.description.len() > 40 {
                self.name_lbl.set_tooltip_text(Some(&s.description));
            } else {
                self.name_lbl.set_tooltip_text(None);
            }
            *self.cached_desc.borrow_mut() = desc;
        }
    }

    fn update(&self, s: &Source) {
        self.apply_meta(s);

        if echo_settled(&self.pending_mute, s.muted, |a, b| a == b) {
            self.muted_cell.set(s.muted);
            toggle_class(&self.mute_btn, "muted", s.muted);
        }

        if echo_settled(&self.pending_volume, s.volume, |a, b| (a - b).abs() < ECHO_TOLERANCE)
            && (self.slider.value() - s.volume).abs() > SLIDER_NOOP_TOLERANCE
        {
            self.slider.set_value(s.volume);
        }
    }
}

fn build_source_list() -> gtk::ListBox {
    let list = boxed_list();
    let rows: Rc<RefCell<HashMap<String, SourceRow>>> = Rc::new(RefCell::new(HashMap::new()));
    let list_for_bind = list.clone();
    let rows_for_bind = rows.clone();
    bind(
        pipewire::sources(),
        &list,
        move |_, sources: Vec<Source>| {
            let mut rows = rows_for_bind.borrow_mut();
            let new_keys: HashSet<&str> = sources.iter().map(|s| s.name.as_str()).collect();
            let gone: Vec<String> = rows
                .keys()
                .filter(|k| !new_keys.contains(k.as_str()))
                .cloned()
                .collect();
            for k in gone {
                if let Some(r) = rows.remove(&k) {
                    list_for_bind.remove(&r.row);
                }
            }
            for s in &sources {
                if let Some(row) = rows.get(&s.name) {
                    row.update(s);
                } else {
                    let row = SourceRow::new(s);
                    list_for_bind.append(&row.row);
                    rows.insert(s.name.clone(), row);
                }
            }
        },
    );
    list
}

// ── Playback streams ─────────────────────────────────────────────────────────

struct StreamRow {
    row: gtk::ListBoxRow,
    name_lbl: gtk::Label,
    slider: gtk::Scale,
    pending_volume: Rc<Cell<Option<(f64, Instant)>>>,
    mute_btn: gtk::Button,
    muted_cell: Rc<Cell<bool>>,
    pending_mute: Rc<Cell<Option<(bool, Instant)>>>,
    cached_app: RefCell<String>,
}

impl StreamRow {
    fn new(s: &PlaybackStream) -> Self {
        let widget = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        widget.add_css_class("ts-audio-row");

        let spacer = gtk::Label::new(Some("  "));
        spacer.add_css_class("ts-audio-default-btn");
        widget.append(&spacer);

        let initial_app = truncate_desc(&s.app_name);
        let name_lbl = gtk::Label::new(Some(&initial_app));
        name_lbl.set_xalign(0.0);
        name_lbl.set_hexpand(true);
        name_lbl.add_css_class("ts-audio-row-name");
        if s.app_name.len() > 40 {
            name_lbl.set_tooltip_text(Some(&s.app_name));
        }
        widget.append(&name_lbl);

        let slider = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.05);
        slider.set_draw_value(false);
        slider.set_hexpand(true);
        slider.set_size_request(110, -1);
        slider.set_value(s.volume);

        let pending_volume: Rc<Cell<Option<(f64, Instant)>>> = Rc::new(Cell::new(None));
        let pending_for_handler = pending_volume.clone();
        let stream_id = s.id;
        slider.connect_change_value(move |_, _, val| {
            pending_for_handler.set(Some((val, Instant::now())));
            pipewire::set_stream_volume(stream_id, val);
            glib::Propagation::Proceed
        });
        widget.append(&slider);

        let mute_btn = gtk::Button::from_icon_name("audio-volume-muted-symbolic");
        mute_btn.add_css_class("ts-audio-mute-btn");
        let muted_cell = Rc::new(Cell::new(s.muted));
        let pending_mute: Rc<Cell<Option<(bool, Instant)>>> = Rc::new(Cell::new(None));
        let muted_for_click = muted_cell.clone();
        let pending_for_click = pending_mute.clone();
        mute_btn.connect_clicked(move |btn| {
            let new_mute = !muted_for_click.get();
            muted_for_click.set(new_mute);
            pending_for_click.set(Some((new_mute, Instant::now())));
            pipewire::set_stream_mute(stream_id, new_mute);
            toggle_class(btn, "muted", new_mute);
        });
        widget.append(&mute_btn);

        let lbr = gtk::ListBoxRow::new();
        lbr.set_child(Some(&widget));
        StreamRow {
            row: lbr,
            name_lbl,
            slider,
            pending_volume,
            mute_btn,
            muted_cell,
            pending_mute,
            cached_app: RefCell::new(initial_app),
        }
    }

    fn update(&self, s: &PlaybackStream) {
        let app = truncate_desc(&s.app_name);
        if *self.cached_app.borrow() != app {
            self.name_lbl.set_text(&app);
            if s.app_name.len() > 40 {
                self.name_lbl.set_tooltip_text(Some(&s.app_name));
            } else {
                self.name_lbl.set_tooltip_text(None);
            }
            *self.cached_app.borrow_mut() = app;
        }

        if echo_settled(&self.pending_mute, s.muted, |a, b| a == b) {
            self.muted_cell.set(s.muted);
            toggle_class(&self.mute_btn, "muted", s.muted);
        }

        if echo_settled(&self.pending_volume, s.volume, |a, b| (a - b).abs() < ECHO_TOLERANCE)
            && (self.slider.value() - s.volume).abs() > SLIDER_NOOP_TOLERANCE
        {
            self.slider.set_value(s.volume);
        }
    }
}

fn build_playback_list() -> gtk::ListBox {
    let list = boxed_list();
    let rows: Rc<RefCell<HashMap<u32, StreamRow>>> = Rc::new(RefCell::new(HashMap::new()));
    let placeholder_lbl = gtk::Label::new(Some("No active streams"));
    placeholder_lbl.set_xalign(0.0);
    placeholder_lbl.add_css_class("dim-label");
    placeholder_lbl.set_margin_start(12);
    placeholder_lbl.set_margin_end(12);
    placeholder_lbl.set_margin_top(8);
    placeholder_lbl.set_margin_bottom(8);
    let placeholder = gtk::ListBoxRow::new();
    placeholder.set_child(Some(&placeholder_lbl));
    list.append(&placeholder);
    let placeholder_attached = Rc::new(Cell::new(true));

    let list_for_bind = list.clone();
    let rows_for_bind = rows.clone();
    let placeholder_for_bind = placeholder.clone();
    let attached_for_bind = placeholder_attached.clone();
    bind(
        pipewire::playback_streams(),
        &list,
        move |_, streams: Vec<PlaybackStream>| {
            // Toggle the empty-state placeholder.
            if streams.is_empty() && !attached_for_bind.get() {
                list_for_bind.append(&placeholder_for_bind);
                attached_for_bind.set(true);
            } else if !streams.is_empty() && attached_for_bind.get() {
                list_for_bind.remove(&placeholder_for_bind);
                attached_for_bind.set(false);
            }

            let mut rows = rows_for_bind.borrow_mut();
            let new_ids: HashSet<u32> = streams.iter().map(|s| s.id).collect();
            let gone: Vec<u32> = rows
                .keys()
                .copied()
                .filter(|id| !new_ids.contains(id))
                .collect();
            for id in gone {
                if let Some(r) = rows.remove(&id) {
                    list_for_bind.remove(&r.row);
                }
            }
            for s in &streams {
                if let Some(row) = rows.get(&s.id) {
                    row.update(s);
                } else {
                    let row = StreamRow::new(s);
                    list_for_bind.append(&row.row);
                    rows.insert(s.id, row);
                }
            }
        },
    );
    list
}
