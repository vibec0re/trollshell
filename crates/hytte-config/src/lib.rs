//! The GTK-free half of trollshell's user configuration: how
//! `~/.config/trollshell/*` is read and written, and the `places.toml` model.
//!
//! # Why this is its own crate (#640)
//!
//! `places.toml` has **two** writers by design — the operator editing it in
//! `$EDITOR` (which #703 explicitly asked to keep) and the control center's
//! places editor. Two writers over one file must agree byte for byte on how it
//! is validated and rendered, or the "format-preserving" guarantee the issue
//! settled on is only true of whichever one happened to write last.
//!
//! The shell's own copy of that logic lived in `hytte-services`, and
//! `trollshell-control-center` cannot link `hytte-services`: it pulls `gtk`,
//! `pipewire` and `hytte-ecal`, i.e. libpipewire and evolution-data-server into
//! a settings app. So the shared half moved here, to a leaf crate that depends
//! on nothing but `serde`/`toml`/`toml_edit`/`tracing` — the same shape as
//! `hytte-plugin-proto` and `hytte-ai-providers`. `hytte-services::places` now
//! wraps this with the reactive/service layer, and the control center calls it
//! directly. One model, one validator, one writer.
//!
//! - [`file`](mod@file) — the `~/.config/trollshell/<name>` path/read/write boilerplate,
//!   including the workspace's single copy of the atomic tmp + `fsync` +
//!   `rename(2)` replacement (#733/#739).
//! - [`places`](mod@places) — the `places.toml` schema, its validation rules, and the
//!   format-preserving writer.

pub mod file;
pub mod places;
