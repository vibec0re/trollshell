//! Public data shapes for the `com.canonical.dbusmenu` menu tree.
//!
//! These are the typed results produced by the pure parsers in
//! [`super::parse`]; they carry no D-Bus state and are re-exported from
//! [`super`] so the public path (`hytte_services::tray::Menu`, …) is
//! unchanged by the module split.

/// A `DBusMenu` tree fetched from one `com.canonical.dbusmenu` endpoint.
#[derive(Clone, Debug)]
pub struct Menu {
    pub id: i32,
    pub items: Vec<MenuEntry>,
}

/// A single entry in a [`Menu`].
#[derive(Clone, Debug)]
pub enum MenuEntry {
    Item(MenuItem),
    Separator,
}

/// Toggle style for a [`MenuItem`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToggleType {
    None,
    Checkmark,
    Radio,
}

/// A menu item fetched from `com.canonical.dbusmenu`.
#[derive(Clone, Debug)]
pub struct MenuItem {
    pub id: i32,
    /// Display label with accelerator markers stripped.
    pub label: String,
    pub enabled: bool,
    pub icon_name: String,
    pub toggle_type: ToggleType,
    /// 0 = unchecked, 1 = checked, -1 = indeterminate.
    pub toggle_state: i32,
    /// Sub-items when `children-display == "submenu"`.
    pub submenu: Option<Vec<MenuEntry>>,
}
