//! `Bar` — a top/bottom/left/right layer-shell strip with three widget
//! groups (left/center/right). Built on `LayerWindow`.
//!
//! Returns a `BarHandle` which keeps the underlying window alive; dropping
//! it closes the bar.

use crate::Monitor;
use crate::layer_window::{Anchor, Margin, layer_window};
use gtk::prelude::*;
use gtk4_layer_shell::KeyboardMode;

/// Which screen edge a [`Bar`] is docked to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Edge {
    /// Dock along the top edge.
    Top,
    /// Dock along the bottom edge.
    Bottom,
    /// Dock along the left edge.
    Left,
    /// Dock along the right edge.
    Right,
}

/// Builder for a layer-shell status bar. Start from [`Bar::new`], set the
/// edge / margin / widget groups, then [`Bar::build`] to present it.
pub struct Bar {
    monitor: Monitor,
    edge: Edge,
    margin: Margin,
    exclusive: bool,
    keyboard_mode: Option<KeyboardMode>,
    left: Vec<gtk::Widget>,
    center: Vec<gtk::Widget>,
    right: Vec<gtk::Widget>,
}

impl Bar {
    /// Start building a bar on `monitor` (defaults: top edge, exclusive zone
    /// on, no margin, keyboard input off).
    #[must_use]
    pub fn new(monitor: &Monitor) -> Self {
        Self {
            monitor: monitor.clone(),
            edge: Edge::Top,
            margin: Margin::default(),
            exclusive: true,
            keyboard_mode: None,
            left: Vec::new(),
            center: Vec::new(),
            right: Vec::new(),
        }
    }

    /// Dock the bar to `edge` (top/bottom/left/right).
    #[must_use]
    pub fn edge(mut self, edge: Edge) -> Self {
        self.edge = edge;
        self
    }

    /// Set the gaps between the bar and the screen edges it spans.
    #[must_use]
    pub fn margin(mut self, m: Margin) -> Self {
        self.margin = m;
        self
    }

    /// Reserve an exclusive zone so tiled windows don't overlap the bar
    /// (on by default).
    #[must_use]
    pub fn exclusive(mut self, on: bool) -> Self {
        self.exclusive = on;
        self
    }

    /// Enable keyboard input for popovers spawned from bar widgets.
    ///
    /// Layer-shell surfaces have `KeyboardMode::None` by default, which
    /// means popovers parented to a bar widget will not receive keyboard
    /// input. Pass `KeyboardMode::OnDemand` to allow popovers to grab
    /// focus when shown.
    #[must_use]
    pub fn keyboard_interactivity(mut self, mode: KeyboardMode) -> Self {
        self.keyboard_mode = Some(mode);
        self
    }

    /// Append widgets to the leading (left / top) group. Repeatable.
    #[must_use]
    pub fn left(mut self, widgets: impl IntoIterator<Item = gtk::Widget>) -> Self {
        self.left.extend(widgets);
        self
    }

    /// Append widgets to the center group. Repeatable.
    #[must_use]
    pub fn center(mut self, widgets: impl IntoIterator<Item = gtk::Widget>) -> Self {
        self.center.extend(widgets);
        self
    }

    /// Append widgets to the trailing (right / bottom) group. Repeatable.
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

        let mut builder = layer_window(&self.monitor)
            .anchor(anchor_main)
            .anchor(anchor_perp.0)
            .anchor(anchor_perp.1)
            .margin(self.margin)
            .exclusive(self.exclusive)
            .namespace(format!("hytte-bar-{:?}", self.edge).to_lowercase());
        if let Some(mode) = self.keyboard_mode {
            builder = builder.keyboard_mode(mode);
        }
        let window = builder.build();
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

        // Pack the center group immediately to the left of the right group
        // (instead of in the geometric middle of the bar). The right group
        // still visually sits on the bar edge via the CenterBox end slot.
        let end_pair = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        end_pair.append(&middle);
        end_pair.append(&right);

        center_box.set_start_widget(Some(&left));
        center_box.set_end_widget(Some(&end_pair));

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
        self.window.destroy();
    }

    /// Access the underlying layer-shell window — useful for binding
    /// state-driven CSS classes from outside the Bar builder.
    #[must_use]
    pub fn window(&self) -> &gtk::Window {
        &self.window
    }
}

impl Drop for BarHandle {
    fn drop(&mut self) {
        self.window.destroy();
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

// #638: `BarHandle`'s teardown paths (`close()`, `Drop`) call `destroy()`
// rather than `close()` specifically because `close()` is a *request* that
// GTK only turns into a real teardown for a *realized* window — see GTK
// 4.22's own GIR docs for `gtk_window_close` vs. `gtk_window_destroy`. A
// `BarHandle` dropped before its bar was ever shown (an error path during
// monitor setup, or a bar built for a monitor that vanished mid-setup) holds
// an unrealized window, which is exactly the case `close()` cannot dispose
// of. This needs a live display (`system-tests`, run under `xvfb-run`); it
// does not need `gtk4-layer-shell` or a compositor, since the
// realized/unrealized distinction it exercises is a plain-GTK toplevel-list
// mechanic, not a layer-shell one — a bare `gtk::Window` stands in fine.
#[cfg(all(test, feature = "system-tests"))]
mod tests {
    use super::*;

    #[gtk::test]
    fn drop_destroys_an_unrealized_bar_window() {
        let window = gtk::Window::new();
        assert!(
            !window.is_realized(),
            "a freshly constructed, never-presented window must start unrealized"
        );
        let weak = window.downgrade();

        // `BarHandle`'s `window` field is private, but this test module is a
        // child of `bar`, so the struct-literal is reachable — no need for a
        // full `Bar::show()` (which would present the window, realizing it).
        let handle = BarHandle { window };
        drop(handle);

        // Let any deferred destroy settle.
        while gtk::glib::MainContext::default().iteration(false) {}

        assert!(
            weak.upgrade().is_none(),
            "dropping a BarHandle must destroy its window even when it was \
             never realized — a bare close() would silently leave it alive",
        );
    }
}
