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
//! $XDG_CONFIG_HOME/trollshell/anthropic.key      secrets, unchanged
//! ```
//!
//! Secrets stay out of the TOML on purpose: #752 established that an API key
//! in the environment is a hazard, and a key in a file the control center can
//! edit would undo that. `*.key` files keep their own path (see
//! `hytte-ai-providers`) — since #1330 retired the general per-provider
//! `<name>.key` fallback, `anthropic.key` (`hytte-claude-bridge`'s own key,
//! not a fallback) is the only one left — and nothing here reads or writes
//! one.
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
//! - [`schema`](mod@schema) — what a family's individual **leaves** are
//!   ([`schema::Field`] / [`schema::Kind`]) and the walker that holds the
//!   declaration to the family's own `DEFAULT_TOML` ([`schema::verify`]), so a
//!   settings UI can render one row per leaf without the shell (#888 P0).
//! - [`state`](mod@state) — the `$XDG_STATE_HOME` writer.
//!
//! # The two cargo features (#1044)
//!
//! Both are **off by default**, and both exist to keep the dependency list
//! above true for the crate's other consumer: `trollshell-control-center` links
//! this crate for its Places tab and must not grow a runtime it never drives.
//!
//! - **`watch`** — `subsystem::watch`, the layer poller that turns a saved
//!   edit into a live reload. Adds `tokio` and `futures-signals`, because the
//!   loop sleeps and publishes into a `Mutable`. The shell's dependency line
//!   enables it; the control center's does not. Spelled as code rather than
//!   linked (#1367): the module is `#[cfg(feature = "watch")]`, so an
//!   intra-doc link to it is a `broken_intra_doc_links` warning in any build
//!   that does not enable the feature — which is every `cargo doc -p
//!   hytte-config`, and invisible to `checks.rustdoc` only because a
//!   `--workspace` run unifies the feature in from the shell's dependency
//!   line.
//! - **`test-support`** — `test_support`, the process-wide tracing global
//!   default plus the capture, scratch-overlay and scratch-`$HOME` harnesses.
//!   A cargo feature rather than `#[cfg(test)]` because `#[cfg(test)]` is
//!   per-crate, and the tree had grown three incompatible copies of the same
//!   `tracing` fix that way — and, by #1226, seven of the scratch `$HOME`.
//!   Enabled by a **dev**-dependency only, so it never reaches a shipped
//!   binary.

pub mod file;
pub mod merge;
pub mod places;
pub mod schema;
pub mod state;
pub mod subsystem;
pub mod xdg;

// The two TOML crates this one's public API is written in, re-exported so a
// consumer needs neither in its own manifest (#1360 review, HIGH 3).
//
// The seam is unavoidable and it is in the *middle* of what a settings form
// does on every row: [`subsystem::Raw::value`] hands back a `toml::Value`,
// [`schema::Kind::accepts`] and [`subsystem::save_leaf_to_locked`] take a
// `toml_edit::Value`, and [`subsystem::to_edit`] is the one conversion between
// them. Without these lines `trollshell-control-center` would have had to add
// two direct dependencies and hand-write a second `to_edit` — which is exactly
// the drift that function's own doc argues against.
//
// A re-export rather than a wrapper type: the values are the caller's own
// data, and a newtype over `toml::Value` would be a third representation to
// convert through.
pub use toml;
pub use toml_edit;

// Test-only plumbing (#1022/#1043/#1044/#1233): the process-wide tracing global
// default, the capture harness, the scratch-overlay helper and the scratch
// `$HOME`. `cfg(test)` as
// well as the feature, so this crate's own suite — and `subsystem::assemble`'s
// `#[cfg(test)]` hook into it — need no feature flag of their own, while a
// dependent's test binary reaches the same code through `test-support`. Absent
// from every non-test build, which is what keeps `set_global_default` out of
// the shipped rlib.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
