//! Cross-cutting building blocks reused across panels and (sometimes)
//! widgets. Each submodule owns one focused helper or a tight family of
//! helpers. Visibility is `pub(crate)` throughout — these are
//! implementation details of the trollshell binary.

pub mod cast;
pub mod chip;
pub mod connection_row;
pub mod deep_link_row;
pub mod focus;
pub mod format;
pub mod history_row;
pub mod layout;
pub mod mpris_controls;
