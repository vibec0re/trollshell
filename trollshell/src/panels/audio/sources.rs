//! Input sources: list of audio inputs with default-radio, volume slider, and mute.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Instant;

use hytte::gtk::{self, glib, prelude::*};
use hytte::prelude::*;
use hytte::services::pipewire::{self, Source};

use super::{
    ECHO_TOLERANCE, SLIDER_NOOP_TOLERANCE, boxed_list, default_radio_glyph, echo_settled,
    toggle_class, truncate_desc,
};

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
        slider.set_size_request(crate::scale::scale(110), -1);
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

        if echo_settled(&self.pending_volume, s.volume, |a, b| {
            (a - b).abs() < ECHO_TOLERANCE
        }) && (self.slider.value() - s.volume).abs() > SLIDER_NOOP_TOLERANCE
        {
            self.slider.set_value(s.volume);
        }
    }
}

pub(super) fn build_source_list() -> gtk::ListBox {
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
