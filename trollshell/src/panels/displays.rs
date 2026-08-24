//! Drawer panel listing the connected outputs as reported by niri's IPC.
//! Each row shows make/model + the active mode and scale, with a switch
//! suffix that calls `displays::set_output_enabled`. Topology changes
//! (plug/unplug) are picked up by the displays service's 2 s polling loop
//! and trigger a full row-list rebuild here.
//!
//! v1 is read-only beyond the on/off switch — there's no mode picker, no
//! scale editor, no rotation UI, and no position editor. Persistent
//! multi-monitor layouts live in `~/.config/kanshi/config` (see
//! `etc/kanshi/`); this page reflects whatever the running compositor +
//! kanshi have already decided.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use hytte::adw::{self, prelude::*};
use hytte::gtk::{self, glib};
use hytte::services::displays::{self, Output};

use crate::components::layout::{finish_page, page_box};
use crate::components::markup;
use crate::components::reactive_list::reactive_list;

pub fn panel_displays() -> gtk::Widget {
    let column = page_box();
    column.add_css_class("ts-popup-column");

    let group = adw::PreferencesGroup::builder().title("Displays").build();
    // In-flight set_output_enabled calls. The displays service polls niri
    // every 2 s; if niri's ack lags the next poll, the rebuilt row would
    // briefly flip the Switch back to the stale state. While a name is in
    // this map, the row build uses the stored intent instead of the
    // service snapshot, so the user's last toggle stands until niri
    // catches up.
    let pending: Rc<RefCell<HashMap<String, bool>>> = Rc::new(RefCell::new(HashMap::new()));

    column.append(&group);

    reactive_list(
        &group,
        displays::outputs(),
        move |output: &Output| build_display_row(output, &pending),
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

fn build_display_row(o: &Output, pending: &Rc<RefCell<HashMap<String, bool>>>) -> adw::ActionRow {
    // Title prefers make+model when EDID is informative; falls back to the
    // bare connector name (e.g. virtual outputs, generic displays).
    let trimmed = format!("{} {}", o.make.trim(), o.model.trim());
    let title = if trimmed.trim().is_empty() {
        o.name.clone()
    } else {
        trimmed.trim().to_string()
    };

    let subtitle = if !o.enabled {
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

    let switch = gtk::Switch::new();
    switch.set_valign(gtk::Align::Center);
    // Skip the active-state restore while a toggle for this output is
    // in-flight: the next poll's row rebuild may carry the pre-toggle
    // state if niri's ack hasn't landed yet, which would visually flip
    // the switch back until the t+2 s poll catches up. When pending,
    // honor the user's last intent instead of the (possibly stale)
    // service snapshot.
    let pending_state = pending.borrow().get(&o.name).copied();
    switch.set_active(pending_state.unwrap_or(o.enabled));
    let name_for_toggle = o.name.clone();
    let pending_for_toggle = pending.clone();
    switch.connect_active_notify(move |sw| {
        let on = sw.is_active();
        pending_for_toggle
            .borrow_mut()
            .insert(name_for_toggle.clone(), on);
        displays::set_output_enabled(&name_for_toggle, on);
        // Disarm after 3 s — long enough for niri to ack and one or two
        // polls to land carrying the new state. After the timeout the
        // service's snapshot is treated as truth again.
        let pending_for_clear = pending_for_toggle.clone();
        let name_for_clear = name_for_toggle.clone();
        glib::timeout_add_local_once(std::time::Duration::from_secs(3), move || {
            pending_for_clear.borrow_mut().remove(&name_for_clear);
        });
    });
    row.add_suffix(&switch);
    row.set_activatable_widget(Some(&switch));

    row
}
