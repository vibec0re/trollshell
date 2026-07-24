//! Shared `~/.config/trollshell/*` persistence boilerplate.
//!
//! Every UI-state service that persists a toggle or a small list to the user's
//! config dir repeats the same three steps: resolve
//! `~/.config/trollshell/<file>`, `mkdir -p` the parent, and read/write with a
//! best-effort `warn!` on failure. These helpers hold that boilerplate in one
//! place; each caller keeps its own (differing) parse/serialize logic.
//!
//! Writes are deliberately best-effort — a failed write logs and returns rather
//! than erroring, because the in-memory `Mutable` is the source of truth for the
//! running process; persistence is a convenience for the *next* launch.

use std::path::PathBuf;

/// Directory (relative to `$HOME`) all trollshell config files live under.
const CONFIG_SUBDIR: &str = ".config/trollshell";

/// Absolute path to `~/.config/trollshell/<file>`. `None` if `$HOME` is unset.
pub(crate) fn path(file: &str) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(CONFIG_SUBDIR).join(file))
}

/// Read `~/.config/trollshell/<file>` as a string, or `None` on any error
/// (missing, unreadable, non-UTF-8) — callers fall back to their default.
pub(crate) fn read(file: &str) -> Option<String> {
    std::fs::read_to_string(path(file)?).ok()
}

/// Write `body` to `~/.config/trollshell/<file>`, creating the parent dir.
///
/// Best-effort: on a `$HOME`-unset / mkdir / write failure it logs a `warn!`
/// scoped to `service` and returns `false`; `true` on success. Simple callers
/// ignore the result (the `Mutable` is authoritative); callers that log their
/// own success line (e.g. `places`' default-config write) read it.
pub(crate) fn write(service: &str, file: &str, body: &str) -> bool {
    let Some(path) = path(file) else {
        tracing::warn!(service, file, "config write skipped: $HOME unset");
        return false;
    };
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        tracing::warn!(service, error = %e, path = %parent.display(), "config mkdir failed");
        return false;
    }
    if let Err(e) = std::fs::write(&path, body) {
        tracing::warn!(service, error = %e, path = %path.display(), "config write failed");
        return false;
    }
    true
}
