//! Audio drawer panel — output sinks, input sources, and per-stream
//! playback volume.
//!
//! Each section is a `boxed-list`-styled `gtk::ListBox` rebuilt on every
//! signal emission from `hytte::services::pipewire`. Rows render the
//! default-radio button, name, volume slider, and mute toggle.

use std::cell::Cell;
use std::rc::Rc;

use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::pipewire::{self, PlaybackStream, Sink, Source};

use crate::components::layout::{finish_page, page_box};

pub fn panel_audio() -> gtk::Widget {
    let column = page_box();

    column.append(&audio_section("Output", &build_sink_list()));
    column.append(&audio_section("Input", &build_source_list()));
    column.append(&audio_section("Playback", &build_playback_list()));

    finish_page(&column)
}

/// Wrap a section title + `boxed-list`-styled `ListBox` in a vertical Box so
/// every audio section has the same Adwaita-style framing.
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

fn build_sink_list() -> gtk::ListBox {
    let list = boxed_list();
    let list_for_bind = list.clone();
    bind(pipewire::sinks(), &list, move |_, sinks: Vec<Sink>| {
        while let Some(child) = list_for_bind.first_child() {
            list_for_bind.remove(&child);
        }
        for s in &sinks {
            list_for_bind.append(&sink_row(s));
        }
    });
    list
}

fn build_source_list() -> gtk::ListBox {
    let list = boxed_list();
    let list_for_bind = list.clone();
    bind(
        pipewire::sources(),
        &list,
        move |_, sources: Vec<Source>| {
            while let Some(child) = list_for_bind.first_child() {
                list_for_bind.remove(&child);
            }
            for s in &sources {
                list_for_bind.append(&source_row(s));
            }
        },
    );
    list
}

fn build_playback_list() -> gtk::ListBox {
    let list = boxed_list();
    let list_for_bind = list.clone();
    bind(
        pipewire::playback_streams(),
        &list,
        move |_, streams: Vec<PlaybackStream>| {
            while let Some(child) = list_for_bind.first_child() {
                list_for_bind.remove(&child);
            }
            if streams.is_empty() {
                let placeholder = gtk::Label::new(Some("No active streams"));
                placeholder.set_xalign(0.0);
                placeholder.add_css_class("dim-label");
                placeholder.set_margin_start(12);
                placeholder.set_margin_end(12);
                placeholder.set_margin_top(8);
                placeholder.set_margin_bottom(8);
                list_for_bind.append(&placeholder);
            } else {
                for s in &streams {
                    list_for_bind.append(&stream_row(s));
                }
            }
        },
    );
    list
}

fn sink_row(s: &Sink) -> gtk::Widget {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.add_css_class("ts-audio-row");
    if s.is_default {
        row.add_css_class("default");
    }

    // Radio indicator / default button.
    let radio_lbl = gtk::Label::new(Some(if s.is_default { "\u{25cf}" } else { "\u{25cb}" }));
    let radio_btn = gtk::Button::new();
    radio_btn.set_child(Some(&radio_lbl));
    radio_btn.add_css_class("ts-audio-default-btn");
    if s.is_default {
        radio_btn.add_css_class("active");
    }
    let sink_name_for_click = s.name.clone();
    radio_btn.connect_clicked(move |_| {
        pipewire::set_default_sink(&sink_name_for_click);
    });
    row.append(&radio_btn);

    // Name / description label.
    let desc = if s.description.len() > 40 {
        format!("{}…", &s.description[..39])
    } else {
        s.description.clone()
    };
    let name_lbl = gtk::Label::new(Some(&desc));
    name_lbl.set_xalign(0.0);
    name_lbl.set_hexpand(true);
    name_lbl.add_css_class("ts-audio-row-name");
    if !s.is_default {
        name_lbl.add_css_class("dim");
    }
    if s.description.len() > 40 {
        name_lbl.set_tooltip_text(Some(&s.description));
    }
    row.append(&name_lbl);

    // Volume slider.
    let slider = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.05);
    slider.set_draw_value(false);
    slider.set_hexpand(true);
    slider.set_size_request(110, -1);
    slider.set_value(s.volume);

    let sink_name_for_slider = s.name.clone();
    slider.connect_value_changed(move |sl| {
        pipewire::set_sink_volume(&sink_name_for_slider, sl.value());
    });
    // No signal bind here — we rebuild the whole row on each poll emission.
    row.append(&slider);

    // Mute button.
    let mute_btn = gtk::Button::from_icon_name("audio-volume-muted-symbolic");
    mute_btn.add_css_class("ts-audio-mute-btn");
    if s.muted {
        mute_btn.add_css_class("muted");
    }
    let muted_cell = Rc::new(Cell::new(s.muted));
    let sink_name_for_mute = s.name.clone();
    mute_btn.connect_clicked(move |btn| {
        let new_mute = !muted_cell.get();
        muted_cell.set(new_mute);
        pipewire::set_sink_mute(&sink_name_for_mute, new_mute);
        if new_mute {
            btn.add_css_class("muted");
        } else {
            btn.remove_css_class("muted");
        }
    });
    row.append(&mute_btn);

    row.upcast()
}

fn source_row(s: &Source) -> gtk::Widget {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.add_css_class("ts-audio-row");
    if s.is_default {
        row.add_css_class("default");
    }

    // Radio indicator / default button.
    let radio_lbl = gtk::Label::new(Some(if s.is_default { "\u{25cf}" } else { "\u{25cb}" }));
    let radio_btn = gtk::Button::new();
    radio_btn.set_child(Some(&radio_lbl));
    radio_btn.add_css_class("ts-audio-default-btn");
    if s.is_default {
        radio_btn.add_css_class("active");
    }
    let source_name_for_click = s.name.clone();
    radio_btn.connect_clicked(move |_| {
        pipewire::set_default_source(&source_name_for_click);
    });
    row.append(&radio_btn);

    // Name / description label.
    let desc = if s.description.len() > 40 {
        format!("{}…", &s.description[..39])
    } else {
        s.description.clone()
    };
    let name_lbl = gtk::Label::new(Some(&desc));
    name_lbl.set_xalign(0.0);
    name_lbl.set_hexpand(true);
    name_lbl.add_css_class("ts-audio-row-name");
    if !s.is_default {
        name_lbl.add_css_class("dim");
    }
    if s.description.len() > 40 {
        name_lbl.set_tooltip_text(Some(&s.description));
    }
    row.append(&name_lbl);

    // Volume slider.
    let slider = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.05);
    slider.set_draw_value(false);
    slider.set_hexpand(true);
    slider.set_size_request(110, -1);
    slider.set_value(s.volume);

    let source_name_for_slider = s.name.clone();
    slider.connect_value_changed(move |sl| {
        pipewire::set_source_volume(&source_name_for_slider, sl.value());
    });
    row.append(&slider);

    // Mute button.
    let mute_btn = gtk::Button::from_icon_name("audio-volume-muted-symbolic");
    mute_btn.add_css_class("ts-audio-mute-btn");
    if s.muted {
        mute_btn.add_css_class("muted");
    }
    let muted_cell = Rc::new(Cell::new(s.muted));
    let source_name_for_mute = s.name.clone();
    mute_btn.connect_clicked(move |btn| {
        let new_mute = !muted_cell.get();
        muted_cell.set(new_mute);
        pipewire::set_source_mute(&source_name_for_mute, new_mute);
        if new_mute {
            btn.add_css_class("muted");
        } else {
            btn.remove_css_class("muted");
        }
    });
    row.append(&mute_btn);

    row.upcast()
}

fn stream_row(s: &PlaybackStream) -> gtk::Widget {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.add_css_class("ts-audio-row");

    // Spacer matching radio button width so labels align with sink/source rows.
    let spacer = gtk::Label::new(Some("  "));
    spacer.add_css_class("ts-audio-default-btn");
    row.append(&spacer);

    // App name label.
    let app_name = if s.app_name.len() > 40 {
        format!("{}…", &s.app_name[..39])
    } else {
        s.app_name.clone()
    };
    let name_lbl = gtk::Label::new(Some(&app_name));
    name_lbl.set_xalign(0.0);
    name_lbl.set_hexpand(true);
    name_lbl.add_css_class("ts-audio-row-name");
    if s.app_name.len() > 40 {
        name_lbl.set_tooltip_text(Some(&s.app_name));
    }
    row.append(&name_lbl);

    // Volume slider.
    let slider = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.05);
    slider.set_draw_value(false);
    slider.set_hexpand(true);
    slider.set_size_request(110, -1);
    slider.set_value(s.volume);

    let stream_id = s.id;
    slider.connect_value_changed(move |sl| {
        pipewire::set_stream_volume(stream_id, sl.value());
    });
    row.append(&slider);

    // Mute button.
    let mute_btn = gtk::Button::from_icon_name("audio-volume-muted-symbolic");
    mute_btn.add_css_class("ts-audio-mute-btn");
    if s.muted {
        mute_btn.add_css_class("muted");
    }
    let muted_cell = Rc::new(Cell::new(s.muted));
    mute_btn.connect_clicked(move |btn| {
        let new_mute = !muted_cell.get();
        muted_cell.set(new_mute);
        pipewire::set_stream_mute(stream_id, new_mute);
        if new_mute {
            btn.add_css_class("muted");
        } else {
            btn.remove_css_class("muted");
        }
    });
    row.append(&mute_btn);

    row.upcast()
}
