//! `Popup` — an anchored popover hosted on a trigger widget.
//!
//! Wraps `gtk::Popover`. The popover is `set_parent(&trigger)`'d so it
//! positions automatically relative to the trigger's allocation. Click
//! outside dismisses (default Popover behaviour).
//!
//! For popups spawned from a `Bar`, the bar must be built with
//! `Bar::keyboard_interactivity(KeyboardMode::OnDemand)` so the layer
//! surface can grant keyboard focus to the popover.

use gtk::prelude::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Position {
    Top,
    Bottom,
    Left,
    Right,
}

pub struct PopupBuilder {
    anchor: gtk::Widget,
    child: Option<gtk::Widget>,
    position: Position,
    has_arrow: bool,
    css_class: Option<String>,
}

impl PopupBuilder {
    #[must_use]
    pub fn child(mut self, child: impl IsA<gtk::Widget>) -> Self {
        self.child = Some(child.upcast());
        self
    }

    #[must_use]
    pub fn position(mut self, position: Position) -> Self {
        self.position = position;
        self
    }

    #[must_use]
    pub fn has_arrow(mut self, on: bool) -> Self {
        self.has_arrow = on;
        self
    }

    #[must_use]
    pub fn css_class(mut self, class: impl Into<String>) -> Self {
        self.css_class = Some(class.into());
        self
    }

    /// Build the popover. The popover is parented to the anchor widget;
    /// dropping the returned handle does *not* close it (the parent owns
    /// it via GTK's reference counting).
    #[must_use]
    pub fn build(self) -> Popup {
        let popover = gtk::Popover::new();
        popover.set_parent(&self.anchor);
        popover.set_position(map_position(self.position));
        popover.set_has_arrow(self.has_arrow);
        popover.set_autohide(true);
        popover.add_css_class("hytte-popup");
        if let Some(class) = self.css_class {
            popover.add_css_class(&class);
        }
        if let Some(child) = self.child {
            popover.set_child(Some(&child));
        }
        Popup { popover }
    }
}

/// Handle to a built popover. Cheap to clone (refcounted `GObject`).
#[derive(Clone)]
pub struct Popup {
    popover: gtk::Popover,
}

impl Popup {
    #[must_use]
    #[allow(clippy::new_ret_no_self)]
    pub fn new(anchor: &impl IsA<gtk::Widget>) -> PopupBuilder {
        PopupBuilder {
            anchor: anchor.clone().upcast(),
            child: None,
            position: Position::Bottom,
            has_arrow: false,
            css_class: None,
        }
    }

    pub fn show(&self) {
        self.popover.popup();
    }

    pub fn hide(&self) {
        self.popover.popdown();
    }

    pub fn toggle(&self) {
        if self.popover.is_visible() {
            self.popover.popdown();
        } else {
            self.popover.popup();
        }
    }

    /// Underlying `gtk::Popover` for advanced use.
    #[must_use]
    pub fn popover(&self) -> &gtk::Popover {
        &self.popover
    }
}

fn map_position(p: Position) -> gtk::PositionType {
    match p {
        Position::Top => gtk::PositionType::Top,
        Position::Bottom => gtk::PositionType::Bottom,
        Position::Left => gtk::PositionType::Left,
        Position::Right => gtk::PositionType::Right,
    }
}
