//! Drawer panel for upcoming calendar events. Renders the same
//! Adwaita-style month grid + upcoming list as the sidebar surface via
//! [`crate::widgets::calendar::widget_for_drawer`] — single source of
//! truth for calendar UI across both surfaces.
//!
//! The drawer's `Page::Calendar` triggers `calendar::refresh()` from
//! `modal::on_page_show` on every open, so the widget doesn't need its
//! own monitor-keyed open hook here.

use hytte::gtk::{self, prelude::*};

use crate::components::layout::{finish_page, page_box};

pub fn panel_calendar() -> gtk::Widget {
    let column = page_box();
    column.add_css_class("ts-popup-column");
    column.append(&crate::widgets::calendar::widget_for_drawer());
    finish_page(&column)
}
