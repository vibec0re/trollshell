//! Drawer panel listing the connected outputs as reported by niri's IPC.
//! Each row shows make/model + the active mode and scale, with a switch
//! suffix that calls `displays::set_output_enabled`. Topology changes
//! (plug/unplug) are picked up by the displays service's 2 s polling loop
//! and trigger a full row-list rebuild here.
//!
//! The rows are **stateless**: everything a row renders, including whether a
//! toggle is still in flight, comes out of the `Output` the service emitted.
//! This page used to keep its own `Rc<RefCell<HashMap<String, bool>>>` of
//! in-flight intent plus a 3 s `glib` timeout to disarm it, which is the model
//! #599 retired — clearing that map re-rendered nothing, so a toggle niri never
//! honoured left the switch pinned in the user's position with no way back, and
//! two quick toggles of one connector had the first one's timer disarm the
//! second's intent early. The service owns it now (`displays::Output::enabled`
//! is a `Pending<bool>`), which also means every monitor's drawer agrees and a
//! drawer rebuild loses nothing.
//!
//! v1 is read-only beyond the on/off switch — there's no mode picker, no
//! scale editor, no rotation UI, and no position editor. Persistent
//! multi-monitor layouts live in `~/.config/kanshi/config` (see
//! `etc/kanshi/`); this page reflects whatever the running compositor +
//! kanshi have already decided.

use hytte::adw::{self, prelude::*};
use hytte::gtk;
use hytte::services::displays::{self, Output};

use crate::components::layout::{finish_page, page_box};
use crate::components::markup;
use crate::components::reactive_list::reactive_list;

pub fn panel_displays() -> gtk::Widget {
    let column = page_box();
    column.add_css_class("ts-popup-column");

    let group = adw::PreferencesGroup::builder().title("Displays").build();

    column.append(&group);

    reactive_list(
        &group,
        displays::outputs(),
        build_display_row,
        // Show a single placeholder row when niri reports nothing yet — typically
        // the first poll hasn't completed, or niri's IPC socket isn't reachable.
        Some(|| {
            adw::ActionRow::builder()
                .title("No displays detected")
                .subtitle("Waiting for niri\u{2026}")
                .activatable(false)
                .build()
        }),
    );

    finish_page(&column)
}

fn build_display_row(o: &Output) -> adw::ActionRow {
    // Title prefers make+model when EDID is informative; falls back to the
    // bare connector name (e.g. virtual outputs, generic displays).
    let trimmed = format!("{} {}", o.make.trim(), o.model.trim());
    let title = if trimmed.trim().is_empty() {
        o.name.clone()
    } else {
        trimmed.trim().to_string()
    };

    // While a toggle is un-echoed the mode/scale line describes the state the
    // user is leaving, so say what is happening instead — the same "name the
    // wait" move the Night light row makes (#597/#599). niri normally acks
    // within a poll, so this is a beat, not a screen.
    let subtitle = if o.enabled.is_pending() {
        if *o.enabled.displayed() {
            "Turning on\u{2026}".to_string()
        } else {
            "Turning off\u{2026}".to_string()
        }
    } else if !*o.enabled.confirmed() {
        "Disabled".to_string()
    } else if let Some(mode) = o.mode {
        let hz = f64::from(mode.refresh_mhz) / 1000.0;
        format!(
            "{}\u{00d7}{} @ {:.0} Hz, {:.1}\u{00d7}",
            mode.width, mode.height, hz, o.scale,
        )
    } else {
        // Enabled but no mode reported — odd but possible during
        // mode-switch races. Show what we know.
        format!("Enabled, scale {:.1}\u{00d7}", o.scale)
    };

    let row = adw::ActionRow::builder()
        .title(&title)
        .subtitle(&subtitle)
        .build();
    // Make/model come straight out of the monitor's EDID (#753).
    markup::plain_text(&row);
    // Long-form titles or driver-injected EDIDs can blow the modal width;
    // let title and subtitle wrap rather than push the surface.
    row.set_subtitle_lines(0);

    let prefix = gtk::Label::new(Some(&o.name));
    prefix.add_css_class("ts-display-connector");
    prefix.add_css_class("monospace");
    prefix.set_valign(gtk::Align::Center);
    row.add_prefix(&prefix);

    // The spinner goes before the switch: `add_suffix` appends, and the
    // progress cue belongs between the title and the control (the Night light
    // row's shape). No binding — the row is rebuilt from scratch on every
    // emission, so the state it was built from is the current one.
    if o.enabled.is_pending() {
        let spinner = gtk::Spinner::new();
        spinner.set_valign(gtk::Align::Center);
        spinner.set_spinning(true);
        row.add_suffix(&spinner);
    }

    let switch = gtk::Switch::new();
    switch.set_valign(gtk::Align::Center);
    // `displayed()`, not `confirmed()`: while a toggle is un-echoed this is the
    // position the user put the switch in, and rebuilding the row from niri's
    // not-yet-updated reading is exactly how the switch used to flip back on
    // its own. Set before connecting, so it can't re-enter the handler below.
    switch.set_active(*o.enabled.displayed());
    let name = o.name.clone();
    switch.connect_active_notify(move |sw| {
        // Fire-and-forget; the service records the intent and owns retiring it.
        displays::set_output_enabled(&name, sw.is_active());
    });
    row.add_suffix(&switch);
    row.set_activatable_widget(Some(&switch));

    row
}
