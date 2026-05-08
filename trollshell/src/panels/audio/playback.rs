//! Playback streams: per-app slider/mute, with empty-state placeholder.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Instant;

use hytte::gtk::{self, glib, prelude::*};
use hytte::prelude::*;
use hytte::services::pipewire::{self, PlaybackStream};

use super::{
    ECHO_TOLERANCE, SLIDER_NOOP_TOLERANCE, boxed_list, echo_settled, toggle_class, truncate_desc,
};

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

pub(super) fn build_playback_list() -> gtk::ListBox {
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
