//! Bundled asset path resolution.
//!
//! Resolution order, highest priority first:
//!
//! 1. `TROLLSHELL_DATA_DIR` env at runtime (override, e.g. for testing).
//! 2. `TROLLSHELL_DATA_DIR` env at compile time (set by the Nix derivation
//!    to `$out/share/trollshell`).
//! 3. `CARGO_MANIFEST_DIR` (dev fallback — assets sit next to `Cargo.toml`).

use std::path::PathBuf;

const COMPILED_BASE: &str = match option_env!("TROLLSHELL_DATA_DIR") {
    Some(s) => s,
    None => env!("CARGO_MANIFEST_DIR"),
};

#[must_use]
pub fn path(rel: &str) -> PathBuf {
    let base = std::env::var("TROLLSHELL_DATA_DIR").unwrap_or_else(|_| COMPILED_BASE.to_string());
    PathBuf::from(base).join(rel)
}
