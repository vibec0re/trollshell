//! Playback streams: per-app slider/mute, with empty-state placeholder.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Instant;

use hytte::futures_signals::signal::Signal;
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
        slider.set_size_request(crate::scale::scale(110), -1);
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

        if echo_settled(&self.pending_volume, s.volume, |a, b| {
            (a - b).abs() < ECHO_TOLERANCE
        }) && (self.slider.value() - s.volume).abs() > SLIDER_NOOP_TOLERANCE
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

    bind_playback_rows(
        &list,
        pipewire::playback_streams(),
        &rows,
        &placeholder,
        &placeholder_attached,
    );
    list
}

/// Diff `signal`'s playback streams into `list`, keeping the live rows in
/// `rows` and toggling the empty-state `placeholder` (whose attachment is
/// tracked in `placeholder_attached`).
///
/// Split out of [`build_playback_list`] so this `bind` call site's `WeakRef`
/// contract (#224, `hytte-reactive/src/bind.rs:16-22`) can be driven with a
/// synthetic signal in tests, the way `build_endpoint_list`'s sibling site
/// already is. The builder reads `pipewire::playback_streams()` inline, which
/// `.expect()`s without a registered `Registry` (#831).
///
/// `placeholder` is `bind`'s second-widget carve-out (#772): it is a
/// `gtk::ListBoxRow` *child*, never the bound `gtk::ListBox`, so capturing it
/// strongly is correct rather than a pin — `nix/lint-bind-pins.py`'s header
/// names this exact site.
fn bind_playback_rows<S>(
    list: &gtk::ListBox,
    signal: S,
    rows: &Rc<RefCell<HashMap<u32, StreamRow>>>,
    placeholder: &gtk::ListBoxRow,
    placeholder_attached: &Rc<Cell<bool>>,
) where
    S: Signal<Item = Vec<PlaybackStream>> + 'static,
{
    let rows_for_bind = rows.clone();
    let placeholder_for_bind = placeholder.clone();
    let attached_for_bind = placeholder_attached.clone();
    bind(signal, list, move |list, streams: Vec<PlaybackStream>| {
        // Toggle the empty-state placeholder.
        if streams.is_empty() && !attached_for_bind.get() {
            list.append(&placeholder_for_bind);
            attached_for_bind.set(true);
        } else if !streams.is_empty() && attached_for_bind.get() {
            list.remove(&placeholder_for_bind);
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
                list.remove(&r.row);
            }
        }
        for s in &streams {
            if let Some(row) = rows.get(&s.id) {
                row.update(s);
            } else {
                let row = StreamRow::new(s);
                list.append(&row.row);
                rows.insert(s.id, row);
            }
        }
    });
}

/// #831 regression coverage for this file's widget-pinning `bind` call site,
/// in the shape `panels/connections.rs` established for #772: the apply
/// closure must take the `&gtk::ListBox` `bind` hands it rather than a strong
/// clone captured from the enclosing scope, or the binding keeps the list
/// alive for its own lifetime and defeats #224's `WeakRef` contract
/// (`hytte-reactive/src/bind.rs:16-22`).
#[cfg(all(test, feature = "system-tests"))]
mod pin_tests {
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::rc::Rc;

    use hytte::adw;
    use hytte::futures_signals::signal::Mutable;
    use hytte::gtk::{self, prelude::*};
    use hytte::services::pipewire::PlaybackStream;

    use super::{StreamRow, bind_playback_rows};

    /// Run the GTK main loop until it has nothing left to dispatch.
    fn pump() {
        while gtk::glib::MainContext::default().iteration(false) {}
    }

    /// A `PlaybackStream` distinguished only by `id` — the diff keys on it.
    /// Every field is `pub` (see `hytte_services::pipewire::PlaybackStream`),
    /// so a struct literal needs no constructor of its own.
    fn stream(id: u32) -> PlaybackStream {
        PlaybackStream {
            id,
            app_name: "Test".to_owned(),
            sink_id: 0,
            volume: 0.5,
            muted: false,
        }
    }

    /// The widget and the three cells `build_playback_list` builds, in the
    /// same shapes.
    struct Cells {
        list: gtk::ListBox,
        rows: Rc<RefCell<HashMap<u32, StreamRow>>>,
        placeholder: gtk::ListBoxRow,
        attached: Rc<Cell<bool>>,
    }

    fn cells() -> Cells {
        let list = gtk::ListBox::new();
        let placeholder = gtk::ListBoxRow::new();
        list.append(&placeholder);
        Cells {
            list,
            rows: Rc::new(RefCell::new(HashMap::new())),
            placeholder,
            attached: Rc::new(Cell::new(true)),
        }
    }

    /// Anti-vacuity guard for the pin test below: the binding must actually
    /// apply, or "the widget died" would prove nothing about the closure.
    #[gtk::test]
    fn playback_rows_binding_applies_a_value() {
        adw::init().expect("libadwaita init");
        let Cells {
            list,
            rows,
            placeholder,
            attached,
        } = cells();
        let streams: Mutable<Vec<PlaybackStream>> = Mutable::new(Vec::new());
        bind_playback_rows(
            &list,
            streams.signal_cloned(),
            &rows,
            &placeholder,
            &attached,
        );
        pump();

        streams.set(vec![stream(1)]);
        pump();

        assert_eq!(
            rows.borrow().len(),
            1,
            "the emitted stream must reach the list as a row"
        );
        assert!(
            !attached.get(),
            "a non-empty stream list must detach the empty-state placeholder"
        );
        assert!(
            placeholder.parent().is_none(),
            "a non-empty stream list must actually remove the placeholder from the ListBox, \
             not merely flip `attached`"
        );
        assert!(
            rows.borrow().values().all(|r| r.row.parent().is_some()),
            "each tracked StreamRow must actually be appended to the ListBox"
        );
    }

    /// Falsified by reintroducing the `list_for_bind` strong clone the apply
    /// closure used to capture: with it, `drop(list)` is not the last strong
    /// ref and the weak upgrade still succeeds.
    ///
    /// The `placeholder` clone the closure *does* keep is the #772 carve-out.
    /// The test drops its own `placeholder` handle before the list, so the
    /// widget survives only because the apply closure still holds it — which
    /// is exactly the point: a live carve-out clone does not keep the bind
    /// target alive, because a GTK child holds no reference to its parent.
    #[gtk::test]
    fn playback_rows_binding_does_not_pin_list() {
        adw::init().expect("libadwaita init");
        let Cells {
            list,
            rows,
            placeholder,
            attached,
        } = cells();
        let weak = list.downgrade();
        let streams: Mutable<Vec<PlaybackStream>> = Mutable::new(Vec::new());
        bind_playback_rows(
            &list,
            streams.signal_cloned(),
            &rows,
            &placeholder,
            &attached,
        );
        pump();

        drop(rows);
        drop(placeholder);
        drop(list);

        assert!(
            weak.upgrade().is_none(),
            "bind_playback_rows must not pin its ListBox: a strong clone captured by the apply \
             closure (rather than taking the closure's own `&gtk::ListBox` argument from `bind`) \
             would keep this alive for the life of the binding, defeating #224's WeakRef contract"
        );

        // The binding must release cleanly on the next emission, not panic on
        // a dead weak ref: `bind` upgrades, gets `None`, and breaks its loop.
        streams.set(vec![stream(1)]);
        pump();
    }
}
