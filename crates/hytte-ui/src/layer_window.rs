//! Thin wrapper around `gtk4-layer-shell` that yields a configured
//! `gtk::Window` ready to host shell content.
//!
//! `Bar` (next module) is layered on top of this. Consumers wanting a
//! non-`Bar` layer surface (e.g. an OSD or a wallpaper) can use
//! `LayerWindow` directly.

use crate::Monitor;
use gtk4_layer_shell::{Edge as LsEdge, KeyboardMode, Layer, LayerShell};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Anchor {
    Top,
    Bottom,
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Margin {
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub left: i32,
}

pub struct LayerWindowBuilder {
    monitor: Monitor,
    layer: Layer,
    anchors: Vec<Anchor>,
    margin: Margin,
    namespace: String,
    exclusive: bool,
    keyboard_mode: Option<KeyboardMode>,
}

impl LayerWindowBuilder {
    #[must_use]
    pub fn anchor(mut self, edge: Anchor) -> Self {
        self.anchors.push(edge);
        self
    }

    #[must_use]
    pub fn margin(mut self, m: Margin) -> Self {
        self.margin = m;
        self
    }

    #[must_use]
    pub fn layer(mut self, layer: Layer) -> Self {
        self.layer = layer;
        self
    }

    #[must_use]
    pub fn namespace(mut self, ns: impl Into<String>) -> Self {
        self.namespace = ns.into();
        self
    }

    #[must_use]
    pub fn exclusive(mut self, on: bool) -> Self {
        self.exclusive = on;
        self
    }

    #[must_use]
    pub fn keyboard_mode(mut self, mode: KeyboardMode) -> Self {
        self.keyboard_mode = Some(mode);
        self
    }

    /// Construct the `gtk::Window`, wire up layer-shell, but don't show.
    #[must_use]
    pub fn build(self) -> gtk::Window {
        let window = gtk::Window::new();
        window.init_layer_shell();
        window.set_layer(self.layer);
        window.set_namespace(Some(&self.namespace));
        window.set_monitor(Some(self.monitor.gdk()));

        for anchor in &self.anchors {
            window.set_anchor(map_edge(*anchor), true);
        }

        window.set_margin(LsEdge::Top, self.margin.top);
        window.set_margin(LsEdge::Right, self.margin.right);
        window.set_margin(LsEdge::Bottom, self.margin.bottom);
        window.set_margin(LsEdge::Left, self.margin.left);

        if self.exclusive {
            window.auto_exclusive_zone_enable();
        }

        if let Some(mode) = self.keyboard_mode {
            window.set_keyboard_mode(mode);
        }

        window
    }
}

#[must_use]
pub fn layer_window(monitor: &Monitor) -> LayerWindowBuilder {
    LayerWindowBuilder {
        monitor: monitor.clone(),
        layer: Layer::Top,
        anchors: Vec::new(),
        margin: Margin::default(),
        namespace: String::from("hytte"),
        exclusive: false,
        keyboard_mode: None,
    }
}

fn map_edge(a: Anchor) -> LsEdge {
    match a {
        Anchor::Top => LsEdge::Top,
        Anchor::Bottom => LsEdge::Bottom,
        Anchor::Left => LsEdge::Left,
        Anchor::Right => LsEdge::Right,
    }
}
