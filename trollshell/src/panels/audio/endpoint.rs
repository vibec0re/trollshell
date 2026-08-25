//! Generic keyed-diff row list for pipewire endpoints (sinks & sources).
//!
//! `Sink` and `Source` are structurally identical — id, name, description,
//! volume, muted, `is_default` — and `sinks.rs`/`sources.rs` used to each
//! hand-roll the exact same row (default-radio button, name label, volume
//! slider, mute button) plus the same `HashMap<String, Row>` keyed-diff
//! bookkeeping against a `Vec<T>` snapshot signal, differing only in field
//! access and which trio of `pipewire::set_*` functions to call (#443, the
//! #174 bug-farm pattern). This module is the one shared implementation;
//! `sinks.rs`/`sources.rs` each implement [`Endpoint`] for their pipewire
//! type and instantiate [`build_endpoint_list`] with their three mutation
//! functions.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Instant;

use hytte::futures_signals::signal::Signal;
use hytte::gtk::{self, glib, prelude::*};
use hytte::prelude::*;

use super::{
    ECHO_TOLERANCE, SLIDER_NOOP_TOLERANCE, boxed_list, default_radio_glyph, echo_settled,
    toggle_class, truncate_desc,
};

/// The fields an output/input row needs off a pipewire endpoint snapshot.
/// `name` is the keyed-diff identity (matches every `pipewire::set_*`
/// mutation function's `&str` argument).
pub(super) trait Endpoint {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn volume(&self) -> f64;
    fn muted(&self) -> bool;
    fn is_default(&self) -> bool;
}

struct EndpointRow {
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

impl EndpointRow {
    fn new<T: Endpoint>(
        s: &T,
        set_default: impl Fn(&str) + 'static,
        set_volume: impl Fn(&str, f64) + 'static,
        set_mute: impl Fn(&str, bool) + 'static,
    ) -> Self {
        let widget = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        widget.add_css_class("ts-audio-row");

        let radio_lbl = gtk::Label::new(Some(default_radio_glyph(s.is_default())));
        let radio_btn = gtk::Button::new();
        radio_btn.set_child(Some(&radio_lbl));
        radio_btn.add_css_class("ts-audio-default-btn");
        let name_for_radio = s.name().to_string();
        radio_btn.connect_clicked(move |_| set_default(&name_for_radio));
        widget.append(&radio_btn);

        let initial_desc = truncate_desc(s.description());
        let name_lbl = gtk::Label::new(Some(&initial_desc));
        name_lbl.set_xalign(0.0);
        name_lbl.set_hexpand(true);
        name_lbl.add_css_class("ts-audio-row-name");
        widget.append(&name_lbl);

        let slider = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.05);
        slider.set_draw_value(false);
        slider.set_hexpand(true);
        slider.set_size_request(crate::scale::scale(110), -1);
        slider.set_value(s.volume());

        let pending_volume: Rc<Cell<Option<(f64, Instant)>>> = Rc::new(Cell::new(None));
        let pending_for_handler = pending_volume.clone();
        let name_for_slider = s.name().to_string();
        // `change-value` only fires for user input (drag, scroll, keys);
        // programmatic `set_value` does NOT trigger it, so the
        // snapshot-driven update path can't echo back to pipewire.
        slider.connect_change_value(move |_, _, val| {
            pending_for_handler.set(Some((val, Instant::now())));
            set_volume(&name_for_slider, val);
            glib::Propagation::Proceed
        });
        widget.append(&slider);

        let mute_btn = gtk::Button::from_icon_name("audio-volume-muted-symbolic");
        mute_btn.add_css_class("ts-audio-mute-btn");
        let muted_cell = Rc::new(Cell::new(s.muted()));
        let pending_mute: Rc<Cell<Option<(bool, Instant)>>> = Rc::new(Cell::new(None));
        let muted_for_click = muted_cell.clone();
        let pending_for_click = pending_mute.clone();
        let name_for_mute = s.name().to_string();
        mute_btn.connect_clicked(move |btn| {
            let new_mute = !muted_for_click.get();
            muted_for_click.set(new_mute);
            pending_for_click.set(Some((new_mute, Instant::now())));
            set_mute(&name_for_mute, new_mute);
            toggle_class(btn, "muted", new_mute);
        });
        widget.append(&mute_btn);

        let lbr = gtk::ListBoxRow::new();
        lbr.set_child(Some(&widget));
        let row = EndpointRow {
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

    fn apply_meta<T: Endpoint>(&self, s: &T) {
        toggle_class(&self.widget, "default", s.is_default());
        toggle_class(&self.radio_btn, "active", s.is_default());
        self.radio_lbl.set_text(default_radio_glyph(s.is_default()));
        toggle_class(&self.name_lbl, "dim", !s.is_default());
        let desc = truncate_desc(s.description());
        if *self.cached_desc.borrow() != desc {
            self.name_lbl.set_text(&desc);
            if s.description().len() > 40 {
                self.name_lbl.set_tooltip_text(Some(s.description()));
            } else {
                self.name_lbl.set_tooltip_text(None);
            }
            *self.cached_desc.borrow_mut() = desc;
        }
    }

    fn update<T: Endpoint>(&self, s: &T) {
        self.apply_meta(s);

        if echo_settled(&self.pending_mute, s.muted(), |a, b| a == b) {
            self.muted_cell.set(s.muted());
            toggle_class(&self.mute_btn, "muted", s.muted());
        }

        if echo_settled(&self.pending_volume, s.volume(), |a, b| {
            (a - b).abs() < ECHO_TOLERANCE
        }) && (self.slider.value() - s.volume()).abs() > SLIDER_NOOP_TOLERANCE
        {
            self.slider.set_value(s.volume());
        }
    }
}

/// Build a keyed (by [`Endpoint::name`]) diff `ListBox` bound to `signal`:
/// each emission updates existing rows in place — so an in-progress slider
/// drag survives — rather than tearing down and rebuilding. `set_default`/
/// `set_volume`/`set_mute` are the three `pipewire::set_*` functions for
/// this endpoint kind (sink or source).
pub(super) fn build_endpoint_list<T, S>(
    signal: S,
    set_default: impl Fn(&str) + Clone + 'static,
    set_volume: impl Fn(&str, f64) + Clone + 'static,
    set_mute: impl Fn(&str, bool) + Clone + 'static,
) -> gtk::ListBox
where
    T: Endpoint + 'static,
    S: Signal<Item = Vec<T>> + 'static,
{
    let list = boxed_list();
    let rows: Rc<RefCell<HashMap<String, EndpointRow>>> = Rc::new(RefCell::new(HashMap::new()));
    let rows_for_bind = rows.clone();
    bind(signal, &list, move |list, items: Vec<T>| {
        let mut rows = rows_for_bind.borrow_mut();
        let new_keys: HashSet<&str> = items.iter().map(Endpoint::name).collect();
        let gone: Vec<String> = rows
            .keys()
            .filter(|k| !new_keys.contains(k.as_str()))
            .cloned()
            .collect();
        for k in gone {
            if let Some(r) = rows.remove(&k) {
                list.remove(&r.row);
            }
        }
        for item in &items {
            if let Some(row) = rows.get(item.name()) {
                row.update(item);
            } else {
                let row = EndpointRow::new(
                    item,
                    set_default.clone(),
                    set_volume.clone(),
                    set_mute.clone(),
                );
                list.append(&row.row);
                rows.insert(item.name().to_string(), row);
            }
        }
    });
    list
}

/// #831 regression coverage for this file's widget-pinning `bind` call site,
/// in the shape `panels/connections.rs` established for #772: the apply
/// closure must take the `&gtk::ListBox` `bind` hands it rather than a strong
/// clone captured from the enclosing scope, or the binding keeps the list
/// alive for its own lifetime and defeats #224's `WeakRef` contract
/// (`hytte-reactive/src/bind.rs:16-22`).
///
/// Testable because [`build_endpoint_list`] takes both its signal and its
/// three mutation functions as parameters — a `Mutable` and three no-ops
/// stand in for pipewire, so no `Registry` is needed. Most of #831's other
/// sites read a service accessor inline and have no such seam.
#[cfg(all(test, feature = "system-tests"))]
mod tests {
    use hytte::adw;
    use hytte::futures_signals::signal::Mutable;
    use hytte::gtk::{self, prelude::*};

    use super::{Endpoint, build_endpoint_list};

    /// Run the GTK main loop until it has nothing left to dispatch.
    fn pump() {
        while gtk::glib::MainContext::default().iteration(false) {}
    }

    /// Minimal [`Endpoint`] so the test does not have to construct a real
    /// pipewire `Sink`/`Source`.
    #[derive(Clone)]
    struct FakeEndpoint;

    impl Endpoint for FakeEndpoint {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn description(&self) -> &'static str {
            "Fake endpoint"
        }
        fn volume(&self) -> f64 {
            0.5
        }
        fn muted(&self) -> bool {
            false
        }
        fn is_default(&self) -> bool {
            true
        }
    }

    /// Falsified by reintroducing the `list_for_bind` strong clone the apply
    /// closure used to capture: with it, `drop(list)` is not the last strong
    /// ref and the weak upgrade still succeeds.
    #[gtk::test]
    fn endpoint_list_binding_does_not_pin_list() {
        adw::init().expect("libadwaita init");
        let items: Mutable<Vec<FakeEndpoint>> = Mutable::new(Vec::new());
        let list = build_endpoint_list(items.signal_cloned(), |_| {}, |_, _| {}, |_, _| {});
        let weak = list.downgrade();
        pump();

        drop(list);

        assert!(
            weak.upgrade().is_none(),
            "build_endpoint_list must not pin its ListBox: a strong clone captured by the apply \
             closure (rather than taking the closure's own `&gtk::ListBox` argument from `bind`) \
             would keep this alive for the life of the binding, defeating #224's WeakRef contract"
        );
    }
}
