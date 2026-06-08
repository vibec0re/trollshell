//! Sidebar weather card. Subscribes to `weather::current()` and switches a
//! `gtk::Stack` between Loading / Resolved / Error pages, updating the
//! resolved page's labels in place rather than rebuilding the tree.

use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::weather::{self, WeatherState};

pub fn widget() -> gtk::Widget {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.add_css_class("ts-weather");

    let stack = gtk::Stack::new();
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    stack.set_transition_duration(150);

    // ── Loading ───────────────────────────────────────────────────────────
    let loading = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    loading.set_halign(gtk::Align::Center);
    let spinner = gtk::Spinner::new();
    spinner.start();
    loading.append(&spinner);
    loading.append(&gtk::Label::new(Some("Loading weather…")));
    stack.add_named(&loading, Some("loading"));

    // ── Error ─────────────────────────────────────────────────────────────
    let error = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    error.add_css_class("ts-weather-error");
    error.append(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
    let error_label = gtk::Label::new(None);
    error_label.set_wrap(true);
    error_label.set_xalign(0.0);
    error.append(&error_label);
    stack.add_named(&error, Some("error"));

    // ── Resolved ──────────────────────────────────────────────────────────
    let resolved = gtk::Box::new(gtk::Orientation::Vertical, 0);

    let loc_label = gtk::Label::new(None);
    loc_label.add_css_class("ts-weather-location");
    loc_label.set_halign(gtk::Align::Start);
    resolved.append(&loc_label);

    // Two columns: left = icon/temp/condition/min-max, right = detail rows.
    let columns = gtk::Box::new(gtk::Orientation::Horizontal, 16);
    columns.add_css_class("ts-weather-columns");

    let left = gtk::Box::new(gtk::Orientation::Vertical, 0);
    left.set_valign(gtk::Align::Center);

    let headline = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    headline.add_css_class("ts-weather-headline");
    let cond_icon = gtk::Image::new();
    cond_icon.add_css_class("ts-weather-icon");
    headline.append(&cond_icon);
    let temp_label = gtk::Label::new(None);
    temp_label.add_css_class("ts-weather-temp");
    headline.append(&temp_label);
    left.append(&headline);

    let cond_label = gtk::Label::new(None);
    cond_label.add_css_class("ts-weather-condition");
    cond_label.set_halign(gtk::Align::Start);
    left.append(&cond_label);

    // Today's expected high / low.
    let minmax_label = gtk::Label::new(None);
    minmax_label.add_css_class("ts-weather-minmax");
    minmax_label.set_halign(gtk::Align::Start);
    left.append(&minmax_label);

    columns.append(&left);

    let details = gtk::Box::new(gtk::Orientation::Vertical, 2);
    details.add_css_class("ts-weather-details");
    details.set_hexpand(true);
    details.set_valign(gtk::Align::Center);
    let (feels_row, feels_val) = detail_row("Feels like");
    let (wind_row, wind_val) = detail_row("Wind");
    let (humid_row, humid_val) = detail_row("Humidity");
    details.append(&feels_row);
    details.append(&wind_row);
    details.append(&humid_row);
    columns.append(&details);

    resolved.append(&columns);

    stack.add_named(&resolved, Some("resolved"));
    root.append(&stack);

    // Switch page + repaint labels on each state emission.
    bind(
        weather::current(),
        &stack,
        move |stack, state| match state {
            WeatherState::Loading => stack.set_visible_child_name("loading"),
            WeatherState::Error(msg) => {
                error_label.set_text(&msg);
                stack.set_visible_child_name("error");
            }
            WeatherState::Resolved(s) => {
                loc_label.set_text(&s.location.to_uppercase());
                cond_icon.set_icon_name(Some(s.condition.icon));
                temp_label.set_text(&format!("{:.0}°", s.temp_c));
                cond_label.set_text(s.condition.label);
                minmax_label.set_text(&format!("↑ {:.0}°   ↓ {:.0}°", s.temp_max_c, s.temp_min_c));
                feels_val.set_text(&format!("{:.0}°", s.apparent_c));
                wind_val.set_text(&format!("{:.0} km/h", s.wind_kmh));
                humid_val.set_text(&format!("{}%", s.humidity_pct));
                stack.set_visible_child_name("resolved");
            }
        },
    );

    root.upcast()
}

/// A "label … value" row; the label hexpands so the value pins to the right.
/// Returns the row and the value label (the caller fills the value in).
fn detail_row(label: &str) -> (gtk::Box, gtk::Label) {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    row.add_css_class("ts-weather-detail");
    let name = gtk::Label::new(Some(label));
    name.add_css_class("ts-weather-detail-label");
    name.set_halign(gtk::Align::Start);
    name.set_hexpand(true);
    let value = gtk::Label::new(None);
    value.add_css_class("ts-weather-detail-value");
    value.set_halign(gtk::Align::End);
    row.append(&name);
    row.append(&value);
    (row, value)
}
