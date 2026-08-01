//! The bell chip: unread count, and — since #747 — the tell for a
//! `org.freedesktop.Notifications` name race the shell lost.
//!
//! The contended case is the whole reason the ownership binding is here rather
//! than anywhere else in the shell. `org.freedesktop.Notifications` is a
//! session singleton, so mako or dunst running alongside trollshell means
//! every notification lands there and the bell sits at zero forever, looking
//! perfectly healthy. See [`crate::widgets::contention`] for why this chip and
//! not a toast (a toast would be circular: the daemon that lost the race is
//! the one being asked to draw the complaint).

use hytte::bus::OwnState;
use hytte::gtk::{self, prelude::*};
use hytte::prelude::*;
use hytte::services::notifications;

use crate::widgets::contention::{self, Subject};

/// The words this chip's contended states are rendered with.
const SUBJECT: Subject = Subject {
    headline: "Notifications are not being delivered",
    bus_name: "org.freedesktop.Notifications",
    rival: "another notification daemon (mako, dunst, …)",
};

pub fn widget(monitor: &Monitor) -> gtk::Widget {
    let btn = crate::components::chip::indicator(
        "ts-notif-indicator",
        crate::modal::Page::Notifications,
        monitor,
    );

    let overlay = gtk::Overlay::new();
    let bell = crate::assets::path("icons/notification.svg");
    let icon = gtk::Image::from_file(&bell);
    icon.set_pixel_size(crate::scale::scale(16));
    overlay.set_child(Some(&icon));

    let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    dot.add_css_class("ts-notif-dot");
    dot.set_halign(gtk::Align::End);
    dot.set_valign(gtk::Align::Start);
    overlay.add_overlay(&dot);
    btn.set_child(Some(&overlay));

    bind(notifications::active(), &dot, |w, list| {
        let n = list.len();
        w.set_visible(n > 0);
        if n > 0 {
            w.set_tooltip_text(Some(&n.to_string()));
        } else {
            w.set_tooltip_text(None);
        }
    });

    // Swap the bell for a warning glyph while the name is held by someone
    // else, and put the diagnosis in the chip's tooltip. Deliberately a
    // *structural* change (a different icon) rather than a stylesheet state:
    // it needs no CSS rule to be noticed, and the bell is the exact widget a
    // user stares at while wondering where their notifications went.
    //
    // The bind target is the button because the tooltip belongs to it; the
    // image is captured so the apply-loop dies with the chip either way.
    let icon_for_state = icon.clone();
    bind(
        notifications::ownership(),
        &btn,
        move |b, state: OwnState| {
            if let Some(msg) = contention::notice(&state, &SUBJECT) {
                icon_for_state.set_icon_name(Some(contention::WARN_ICON));
                b.set_tooltip_text(Some(&msg));
            } else {
                icon_for_state.set_from_file(Some(&bell));
                b.set_tooltip_text(None);
            }
        },
    );

    btn.upcast()
}
