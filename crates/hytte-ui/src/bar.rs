//! `Bar` — a top/bottom/left/right layer-shell strip with three widget
//! groups (left/center/right). Built on `LayerWindow`.
//!
//! Returns a `BarHandle` which keeps the underlying window alive; dropping
//! it closes the bar.

use crate::layer_window::{layer_window, Anchor, Margin};
use crate::Monitor;
use gtk::prelude::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Edge {
    Top,
    Bottom,
    Left,
    Right,
}

pub struct Bar {
    monitor: Monitor,
    edge: Edge,
    margin: Margin,
    exclusive: bool,
    left: Vec<gtk::Widget>,
    center: Vec<gtk::Widget>,
    right: Vec<gtk::Widget>,
}

impl Bar {
    #[must_use]
    pub fn new(monitor: &Monitor) -> Self {
        Self {
            monitor: monitor.clone(),
            edge: Edge::Top,
            margin: Margin::default(),
            exclusive: true,
            left: Vec::new(),
            center: Vec::new(),
            right: Vec::new(),
        }
    }

    #[must_use]
    pub fn edge(mut self, edge: Edge) -> Self {
        self.edge = edge;
        self
    }

    #[must_use]
    pub fn margin(mut self, m: Margin) -> Self {
        self.margin = m;
        self
    }

    #[must_use]
    pub fn exclusive(mut self, on: bool) -> Self {
        self.exclusive = on;
        self
    }

    #[must_use]
    pub fn left(mut self, widgets: impl IntoIterator<Item = gtk::Widget>) -> Self {
        self.left.extend(widgets);
        self
    }

    #[must_use]
    pub fn center(mut self, widgets: impl IntoIterator<Item = gtk::Widget>) -> Self {
        self.center.extend(widgets);
        self
    }

    #[must_use]
    pub fn right(mut self, widgets: impl IntoIterator<Item = gtk::Widget>) -> Self {
        self.right.extend(widgets);
        self
    }

    /// Build the bar window, present it, and return a handle that keeps it
    /// alive. Dropping the handle closes the bar.
    #[must_use]
    pub fn show(self) -> BarHandle {
        let (anchor_main, anchor_perp) = perpendicular_anchors(self.edge);

        let window = layer_window(&self.monitor)
            .anchor(anchor_main)
            .anchor(anchor_perp.0)
            .anchor(anchor_perp.1)
            .margin(self.margin)
            .exclusive(self.exclusive)
            .namespace(format!("hytte-bar-{:?}", self.edge).to_lowercase())
            .build();
        window.add_css_class("hytte-bar");
        window.add_css_class(edge_class(self.edge));

        let center_box = gtk::CenterBox::new();
        center_box.add_css_class("hytte-bar-content");

        let left = group_box("hytte-bar-group-left");
        for w in self.left {
            left.append(&w);
        }
        let middle = group_box("hytte-bar-group-center");
        for w in self.center {
            middle.append(&w);
        }
        let right = group_box("hytte-bar-group-right");
        for w in self.right {
            right.append(&w);
        }

        center_box.set_start_widget(Some(&left));
        center_box.set_center_widget(Some(&middle));
        center_box.set_end_widget(Some(&right));

        window.set_child(Some(&center_box));
        window.present();

        BarHandle { window }
    }
}

fn group_box(class: &str) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    b.add_css_class(class);
    b
}

fn perpendicular_anchors(edge: Edge) -> (Anchor, (Anchor, Anchor)) {
    match edge {
        Edge::Top => (Anchor::Top, (Anchor::Left, Anchor::Right)),
        Edge::Bottom => (Anchor::Bottom, (Anchor::Left, Anchor::Right)),
        Edge::Left => (Anchor::Left, (Anchor::Top, Anchor::Bottom)),
        Edge::Right => (Anchor::Right, (Anchor::Top, Anchor::Bottom)),
    }
}

fn edge_class(edge: Edge) -> &'static str {
    match edge {
        Edge::Top => "hytte-bar-top",
        Edge::Bottom => "hytte-bar-bottom",
        Edge::Left => "hytte-bar-left",
        Edge::Right => "hytte-bar-right",
    }
}

/// Holds the bar's underlying window alive. Dropping closes the bar.
pub struct BarHandle {
    window: gtk::Window,
}

impl BarHandle {
    /// Close the bar immediately.
    pub fn close(self) {
        self.window.close();
    }
}

impl Drop for BarHandle {
    fn drop(&mut self) {
        self.window.close();
    }
}

impl BarHandle {
    /// Forget the handle so the bar lives for the application's lifetime.
    /// Useful when constructing many bars in the body closure where you
    /// don't want to track each one individually.
    pub fn into_long_lived(self) {
        std::mem::forget(self);
    }
}
