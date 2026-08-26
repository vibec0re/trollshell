//! Cross-cutting building blocks reused across panels and (sometimes)
//! widgets. Each submodule owns one focused helper or a tight family of
//! helpers. Visibility is `pub(crate)` throughout — these are
//! implementation details of the trollshell binary.

pub mod cast;
pub mod center_budget;
pub mod chip;
pub mod connection_row;
pub mod deep_link_row;
pub mod diff;
pub mod focus;
pub mod focused_output;
pub mod format;
pub mod history_row;
pub mod layout;
pub mod markup;
pub mod monitor_key;
pub mod mpris_controls;
pub mod notif_actions;
pub mod open_refresh;
pub mod power_profile;
pub mod reactive_list;
pub mod visibility_gate;
