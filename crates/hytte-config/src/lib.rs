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
//! # The layering (#868)
//!
//! #866 settled that trollshell's user configuration moves off 43 environment
//! variables and onto per-subsystem TOML, written as a nix **base** that an
//! unmanaged **overlay** layers over, with **state** kept somewhere else
//! entirely. The four modules that make that possible are the second half of
//! this crate, and they are deliberately additive: `places` predates them and
//! goes through none of them, which `tests/places_byte_identical.rs` pins byte
//! for byte.
//!
//! ```text
//! $XDG_CONFIG_DIRS/trollshell/<subsystem>.toml   base, nix-written, read-only
//! $XDG_CONFIG_HOME/trollshell/<subsystem>.toml   overlay, yours
//! $XDG_STATE_HOME/trollshell/<subsystem>.toml    state, the shell's
//! $XDG_CONFIG_HOME/trollshell/*.key              secrets, unchanged
//! ```
//!
//! Secrets stay out of the TOML on purpose: #752 established that an API key
//! in the environment is a hazard, and a key in a file the control center can
//! edit would undo that. `*.key` files keep their own path (see
//! `hytte-ai-providers`), and nothing here reads or writes one.
//!
//! # Modules
//!
//! - [`file`](mod@file) — the `~/.config/trollshell/<name>` path/read/write boilerplate,
//!   including the workspace's single copy of the atomic tmp + `fsync` +
//!   `rename(2)` replacement (#733/#739).
//! - [`places`](mod@places) — the `places.toml` schema, its validation rules, and the
//!   format-preserving writer.
//! - [`xdg`](mod@xdg) — where the layers and the state file live, as pure
//!   functions over an explicit environment.
//! - [`merge`](mod@merge) — the four merge rules (scalars, tables, arrays,
//!   and the spelled-out "unset").
//! - [`subsystem`](mod@subsystem) — the schema shape: declare a type, a name
//!   and a documented default; inherit the reader, the validator harness and
//!   the format-preserving writer.
//! - [`state`](mod@state) — the `$XDG_STATE_HOME` writer.

pub mod file;
pub mod merge;
pub mod places;
pub mod state;
pub mod subsystem;
pub mod xdg;
