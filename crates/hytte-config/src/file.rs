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
//!
//! Writes are also **atomic** (#733): [`write()`] renders into a temp file beside
//! the target and `rename(2)`s over it, so a concurrent reader sees either the
//! whole old file or the whole new one — never a truncated or half-written one.
//! That matters because these files have readers outside this process: the
//! `places` config watcher re-reads on mtime every few seconds, and the
//! `wlsunset` / `swaybg` systemd units read `wlsunset.args` / `swaybg.args`
//! from a `sh -c` wrapper whose `-s` guard catches an empty file but not a
//! partial one.
//!
//! [`write_atomic`] is that algorithm on its own — explicit path, real
//! `io::Error`, no logging — and is the workspace's only copy of it (#739).
//! [`write()`] and `write_path` are the `$HOME`-resolving, `warn!`-logging,
//! `bool`-returning wrapper the UI-state services want; [`crate::places`] calls
//! the core directly because it needs the `io::Error` to build a
//! `PlacesError::Write`. The one axis on which those two callers genuinely
//! differ is [`Durability`], which is therefore a stated parameter rather than
//! a house default — see that type for which caller picks what, and why.

use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::xdg::Env;

/// Directory (relative to `$HOME`) all trollshell config files live under.
const CONFIG_SUBDIR: &str = ".config/trollshell";

/// Symlink hops [`resolve_dangling_target`] will walk by hand, matching
/// Linux's own `MAXSYMLINKS`: a chain exactly this long still resolves, the
/// same as the kernel's own path resolution would follow. One hop longer —
/// which includes a genuine cycle (`a -> b -> a`, even a self-referential
/// `a -> a`), which never terminates at any bound — returns an
/// [`std::io::Error`] instead of guessing.
const MAX_SYMLINK_HOPS: u32 = 40;

/// Distinguishes the temp files of two writes that overlap in time.
///
/// Combined with the pid it is unique across the machine: two threads of this
/// process take different tickets, and no other live process shares our pid.
/// Last writer wins on the target, which is the pre-existing contract; what
/// this prevents is two writers sharing one temp file and interleaving into it.
static TMP_TICKET: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
thread_local! {
    /// Test-only tripwire for the [`Durability::FsyncParent`] branch inside
    /// [`write_atomic`].
    ///
    /// The `fsync` itself is unobservable in-process (its only effect is
    /// durability across a power cut), so nothing here can assert *that* it
    /// ran. What this pins instead is that the branch *fired at all* for a
    /// given call — [`fsync_parent_attempts`] lets a test read the count
    /// before and after a call and assert it moved. That is what
    /// `places::tests::persist_to_pins_the_fsync_parent_durability_choice`
    /// does against the real `places::persist_to`: it fails if the guard
    /// below is inverted (so `FileOnly` writes take the branch instead of
    /// `FsyncParent` ones) just as surely as it fails if `persist_to` is
    /// edited to pass `FileOnly`.
    ///
    /// `thread_local` rather than a shared static: `cargo test` runs test
    /// functions concurrently on a thread pool, and `write_atomic` itself
    /// never spawns, so a plain shared counter would let an unrelated test's
    /// calls on another thread mask a regression on this one (a false pass,
    /// not a flake — the direction that matters least visibly). Confined to
    /// this thread, the only calls that can move the count between a test's
    /// own "before" and "after" reads are that test's own.
    static FSYNC_PARENT_ATTEMPTS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Current value of the tripwire counter above, for this thread.
#[cfg(test)]
pub fn fsync_parent_attempts() -> u64 {
    FSYNC_PARENT_ATTEMPTS.with(std::cell::Cell::get)
}

/// Whether [`write_atomic`] also `fsync`s the parent **directory** after the
/// `rename(2)`.
///
/// The *file's* own `fsync` is unconditional (see `fill`) — without it the
/// rename can be durable while the data isn't, which on a delayed-allocation
/// filesystem resurrects exactly the zero-length file this whole path exists to
/// prevent. Syncing the *directory* on top of that buys something strictly
/// narrower: durability of the rename itself, i.e. "did the write that already
/// returned `Ok` survive a power cut". It costs a second journal commit per
/// write, and losing it leaves the whole *previous* file in place — a state
/// every reader here already handles.
///
/// #739 folded two copies of this algorithm into one, and they disagreed on
/// exactly this call. Making it a named parameter settles the question
/// explicitly instead of inheriting whichever copy happened to survive: both
/// callers keep precisely the behaviour they shipped with, and the next caller
/// has to state its choice rather than get one by accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    /// `fsync` the parent directory after the rename.
    ///
    /// For user-authored data written rarely and deliberately: `places.toml`,
    /// which a person edits by hand or through the editor UI. A save there is
    /// acknowledged to the user, so silently losing an acknowledged one to a
    /// power cut is a data-loss bug rather than a lost toggle, and the extra
    /// commit is amortised over a handful of writes a week.
    ///
    /// Best-effort: some filesystems refuse an `fsync` on a read-only directory
    /// handle, so a failure is ignored — the data is already on disk either
    /// way, only the rename's durability is at stake.
    FsyncParent,
    /// Sync the file's contents only; let the directory entry reach disk
    /// whenever the filesystem gets to it.
    ///
    /// For the click-driven `~/.config/trollshell/*` toggles behind [`write()`]:
    /// they are rewritten constantly, several straight out of a click handler,
    /// and they are explicitly a convenience for the *next* launch — the
    /// in-memory `Mutable` is the source of truth for the running process. A
    /// power cut that costs the last toggle leaves the previous config intact,
    /// so the second journal commit per click isn't worth it.
    FileOnly,
}

/// Absolute path to `~/.config/trollshell/<file>`. `None` if `$HOME` is
/// unset, empty, or itself relative.
///
/// Goes through [`Env::home`] — the same gate [`crate::xdg`] applies to the
/// layered config paths — rather than reading `$HOME` here a second time, so
/// this older, pre-layering helper and `xdg` can't drift apart on what
/// counts as a usable `$HOME` (#985 fixed `xdg`; #1009 is this module
/// catching up to the same rule via the same method instead of a second
/// copy of the check).
#[must_use]
pub fn path(file: &str) -> Option<PathBuf> {
    let env = Env::from_process();
    let home = env.home()?;
    Some(PathBuf::from(home).join(CONFIG_SUBDIR).join(file))
}

/// Read `~/.config/trollshell/<file>` as a string, or `None` on any error
/// (missing, unreadable, non-UTF-8) — callers fall back to their default.
#[must_use]
pub fn read(file: &str) -> Option<String> {
    std::fs::read_to_string(path(file)?).ok()
}

/// Write `body` to `~/.config/trollshell/<file>`, creating the parent dir.
///
/// Atomic, symlink-safe and permission-preserving — see [`write_atomic`], which
/// does the actual work. These files take [`Durability::FileOnly`]: they are
/// click-driven toggles, and the rationale for not paying a directory `fsync`
/// per click is on that variant.
///
/// Best-effort: on a `$HOME`-unset / mkdir / write failure it logs a `warn!`
/// scoped to `service` and returns `false`; `true` on success. Simple callers
/// ignore the result (the `Mutable` is authoritative); callers that log their
/// own success line (e.g. `places`' default-config write) read it.
pub fn write(service: &str, file: &str, body: &str) -> bool {
    let Some(path) = path(file) else {
        tracing::warn!(service, file, "config write skipped: $HOME unset");
        return false;
    };
    write_path(service, &path, body)
}

/// [`write()`] against an already-resolved absolute path — the whole of `write`
/// except the `$HOME` lookup, split out so the tests can drive it against a
/// tempdir without mutating the process environment.
///
/// The algorithm itself is [`write_atomic`]; this is only its
/// `service`-scoped-logging, `bool`-returning skin.
fn write_path(service: &str, path: &Path, body: &str) -> bool {
    match write_atomic(path, body, Durability::FileOnly) {
        Ok(()) => true,
        Err(e) => {
            tracing::warn!(service, error = %e, path = %path.display(), "config write failed");
            false
        }
    }
}

/// Atomically replace `path`'s contents with `body`, creating `path`'s own
/// parent directory (which is not necessarily its resolved target's parent —
/// see Symlink-safe below).
///
/// The workspace's single copy of tmp + `fsync` + `rename(2)` + cleanup (#739);
/// every config file written through [`write()`], plus `places`' own writer,
/// goes through here. No logging and no service scope — the caller decides
/// what a failure means and how to report it.
///
/// The body lands in a temp file in the same directory as the target, is
/// `fsync`ed, and is then `rename(2)`d over it, so no reader ever observes a
/// zero-length or partially-written config, and a crash mid-write leaves the
/// old file whole rather than a truncated one.
///
/// **Symlink-safe:** `path` is `canonicalize`d first, so a `places.toml`
/// symlinked into a dotfiles repo is written *through* rather than replaced by
/// a regular file (#739). That is also what keeps the temp file on the target's
/// own filesystem — `rename(2)` is only atomic within one. A target that
/// doesn't exist at all (no file, no symlink) can't be canonicalised, and
/// needs no resolving — [`resolve_dangling_target`] returns `path` unchanged
/// for that case. A target that exists **as a symlink whose destination
/// hasn't been created yet** also can't be canonicalised — `canonicalize`
/// requires every component including the last to exist — but *does* need
/// resolving: the old fallback of `path` itself `rename(2)`d a regular file
/// over the link and permanently broke a "link first, populate later"
/// dotfiles setup (stow/chezmoi) on its first save (#986).
/// [`resolve_dangling_target`] walks that chain by hand instead — and, since
/// a genuine symlink cycle can never be "resolved" at any bound, returns an
/// error rather than another guess when the chain doesn't end within
/// [`MAX_SYMLINK_HOPS`]; this function propagates it, so every link in a
/// cycle is left exactly as it was rather than one of them being replaced.
///
/// **Permission-safe:** an existing target's mode is carried over at `open`
/// time, before the body is written, so a hand-tightened `0600` config's
/// contents never sit in a briefly umask-default temp file, and the file does
/// not come back `0644` (#739). A brand-new file gets the platform default,
/// exactly as `std::fs::write` would have given it.
///
/// **Durability** of the rename itself is the caller's call — see
/// [`Durability`].
///
/// Any failure removes the temp file rather than leaving litter behind, and
/// leaves the target untouched. From the point `target` is resolved onward,
/// every returned error names that *resolved* target, not just the original
/// `path` — for a symlinked `path` those can differ, and a caller logging
/// `path.display()` alongside the error (e.g. [`write_path`]'s `warn!`)
/// would otherwise have no way to tell which directory actually failed. The
/// error's [`std::io::ErrorKind`] is preserved (only the message is
/// rewritten), so a caller matching on it still can. The `create_dir_all`
/// step runs before `target` is resolved, so its error names `parent` — the
/// original `path`'s directory — instead: a `path` component that already
/// exists as a plain file surfaces as `<parent>: File exists (os error 17)`
/// rather than a pathless one (review follow-up on #1000 / #1009).
pub fn write_atomic(path: &Path, body: &str, durability: Durability) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| std::io::Error::new(e.kind(), format!("{}: {e}", parent.display())))?;
    }

    // Resolve symlinks: `std::fs::write` wrote *through* a symlinked target, so
    // follow it rather than replacing the link with a regular file. It also
    // keeps the temp file next to the real file — `rename(2)` is only atomic
    // within one filesystem. A target that doesn't exist yet can't be
    // canonicalised; `resolve_dangling_target` covers both "no file or link at
    // all" (returns `path` unchanged, same as before) and "a symlink whose
    // destination doesn't exist yet" (follows the link chain by hand) — and
    // errors out, rather than falling back to `path`, on a chain that never
    // terminates (a cycle). Propagate that: no temp file has been created yet,
    // so an early return here leaves every link in the chain untouched.
    let target = match std::fs::canonicalize(path) {
        Ok(target) => target,
        Err(_) => resolve_dangling_target(path)?,
    };
    let dir = target.parent().ok_or_else(|| {
        std::io::Error::other(format!(
            "{}: config path has no parent directory",
            target.display()
        ))
    })?;
    let tmp = dir.join(tmp_name(&target));

    // The mode the target already carries, if any. Applied at `open` so the
    // temp file is never briefly more permissive than the config it replaces.
    let mode = std::fs::metadata(&target)
        .ok()
        .map(|m| m.permissions().mode() & 0o7777);

    let swap = || -> std::io::Result<()> {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        if let Some(mode) = mode {
            opts.mode(mode);
        }
        let mut file = opts.open(&tmp)?;
        let written = fill(&mut file, body, mode);
        drop(file);
        written?;
        std::fs::rename(&tmp, &target)
    };

    if let Err(e) = swap() {
        // Best-effort: a failed write must not leave litter, but the write's
        // own error is what the caller needs to see. The kind is preserved —
        // callers may still match on it — but the message is rewritten to
        // name `target`: for a symlinked `path` that's the *resolved*
        // location, which a bare `path.display()` in the caller's own log
        // line (e.g. `write_path`'s `warn!`) can't show. Without this, a
        // missing directory on the target side of a dangling-symlink write
        // surfaces as a bare "No such file or directory" with nothing to say
        // which directory.
        let _ = std::fs::remove_file(&tmp);
        return Err(std::io::Error::new(
            e.kind(),
            format!("{}: {e}", target.display()),
        ));
    }

    if matches!(durability, Durability::FsyncParent) {
        #[cfg(test)]
        FSYNC_PARENT_ATTEMPTS.with(|c| c.set(c.get() + 1));
        let _ = std::fs::File::open(dir).and_then(|d| d.sync_all());
    }
    Ok(())
}

/// Where a **dangling** symlink at `path` should actually be written.
///
/// Called only after `canonicalize(path)` has already failed. If nothing
/// exists at `path` at all — not even a symlink — `read_link` fails
/// immediately and this returns `path` unchanged, exactly the fallback
/// `write_atomic` used before #986. If `path` is a symlink, this follows
/// `read_link`'s chain by hand — resolving a relative destination against
/// its own link's parent directory, the way any other relative symlink
/// resolves — until it reaches a path that isn't itself a symlink. For a
/// dangling link that endpoint is the never-yet-created target: the same
/// place a working chain would have canonicalized to, had the target
/// existed.
///
/// A relative destination is joined onto its link's parent **lexically**:
/// any `..` component `read_link` handed back is kept exactly as-is, never
/// popped off by hand. A hand-rolled normalizer would be wrong the moment a
/// directory earlier in the path is itself a symlink — `b -> ../c` resolved
/// from inside `a -> real/b` does not generally mean "collapse to `real/c`";
/// which real directory `..` lands in depends on where `real` itself
/// actually is, and only the kernel's own path resolution — which sees the
/// whole filesystem, not just this one link's text — can answer that
/// correctly. Leaving `..` untouched and letting `open(2)`/`rename(2)`
/// resolve the final joined path is what keeps this correct; a lexical
/// `..`-collapsing "fix" here would silently write into the wrong directory.
///
/// # Errors
/// An [`std::io::Error`] (kind [`std::io::ErrorKind::Other`] — `ErrorKind`'s
/// `FilesystemLoop` variant, which would name this precisely, is still
/// unstable on this MSRV) naming the stuck path, if the chain is still a
/// symlink after [`MAX_SYMLINK_HOPS`] hops. A genuine cycle (`a -> b -> a`,
/// even a self-referential `a -> a`) never terminates at any bound, and the
/// pre-#986 behaviour — falling back to `path` in that case — is exactly the
/// bug #986 exists to fix: it would `rename(2)` a regular file over the
/// link. The caller must propagate this rather than swallow it, so every
/// link in a genuine cycle survives the write attempt untouched.
fn resolve_dangling_target(path: &Path) -> std::io::Result<PathBuf> {
    let mut current = path.to_path_buf();
    for _ in 0..MAX_SYMLINK_HOPS {
        match std::fs::read_link(&current) {
            Err(_) => return Ok(current),
            Ok(link_target) => {
                current = if link_target.is_absolute() {
                    link_target
                } else if let Some(parent) = current.parent() {
                    parent.join(&link_target)
                } else {
                    link_target
                };
            }
        }
    }
    // `MAX_SYMLINK_HOPS` links resolved without ever landing on a
    // non-symlink. One more `read_link` distinguishes a chain of *exactly*
    // that length (which must still succeed — the kernel's own resolver
    // would follow it too) from a longer chain or a cycle (which can't be
    // resolved at this bound, or any other).
    if std::fs::read_link(&current).is_err() {
        Ok(current)
    } else {
        Err(std::io::Error::other(format!(
            "{}: symlink chain did not resolve within {} hops (stuck at {})",
            path.display(),
            MAX_SYMLINK_HOPS,
            current.display()
        )))
    }
}

/// Fill the freshly-opened temp file: fix up its mode, write the body, `fsync`.
///
/// The `fsync` is what makes the rename meaningful across a crash — without it
/// the rename can be durable while the data isn't, which on a delayed-
/// allocation filesystem resurrects exactly the zero-length file this is here
/// to prevent. Whether the *directory entry* is synced too is the caller's
/// choice — see [`Durability`].
fn fill(file: &mut std::fs::File, body: &str, mode: Option<u32>) -> std::io::Result<()> {
    // `OpenOptions::mode` only applies when `open` actually creates the file;
    // it is silently ignored if a temp file from a crashed earlier run happened
    // to be sitting at this name, so re-assert it on the fd we ended up with.
    if let Some(mode) = mode {
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    }
    file.write_all(body.as_bytes())?;
    file.sync_all()
}

/// Temp-file name for `target`: hidden, per-process, per-write.
///
/// Leading dot so a stray one (only reachable by crashing mid-write) stays out
/// of the way, and a `.tmp` suffix so it can't collide with a real config name.
fn tmp_name(target: &Path) -> String {
    let stem = target
        .file_name()
        .map_or_else(|| "config".into(), std::ffi::OsStr::to_string_lossy);
    let pid = std::process::id();
    let ticket = TMP_TICKET.fetch_add(1, Ordering::Relaxed);
    format!(".{stem}.{pid}.{ticket}.tmp")
}

/// Delete `~/.config/trollshell/<file>` if it exists.
///
/// Best-effort, like [`write()`]: a missing file is success (nothing to do); a
/// `$HOME`-unset or non-`NotFound` I/O error logs a `warn!` scoped to
/// `service`. Callers use it to return a persisted UI-state toggle to its
/// zero-state (e.g. the wallpaper picker's "Clear" clearing the render files).
pub fn remove(service: &str, file: &str) {
    let Some(path) = path(file) else {
        tracing::warn!(service, file, "config remove skipped: $HOME unset");
        return;
    };
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            tracing::warn!(service, error = %e, path = %path.display(), "config remove failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    // ── `path()`'s `$HOME` resolution agrees with `xdg` (#1009) ────────────
    //
    // `temp_env` (already a dev-dependency for `places`' own `$HOME`-driven
    // tests) serializes the mutation across the whole test binary and
    // restores the previous value afterwards, so these can't race any other
    // test that also touches `$HOME`.

    /// An empty `$HOME` must not resolve to a path relative to the
    /// process's working directory — the same failure #985 fixed in `xdg`,
    /// which `path()` inherited because it read `$HOME` a second time
    /// instead of going through [`Env::home`].
    #[test]
    fn path_rejects_an_empty_home() {
        temp_env::with_var("HOME", Some(""), || {
            assert_eq!(path("dnd.toml"), None);
        });
    }

    /// A relative `$HOME` is exactly as dangerous as an empty one: it would
    /// resolve against the process's working directory (`/` under a
    /// systemd user unit) rather than the user's home.
    #[test]
    fn path_rejects_a_relative_home() {
        temp_env::with_var("HOME", Some("relative-home"), || {
            assert_eq!(path("dnd.toml"), None);
        });
    }

    /// The ordinary case still works: an absolute `$HOME` resolves to
    /// `$HOME/.config/trollshell/<file>`, exactly as before #1009.
    #[test]
    fn path_resolves_an_absolute_home() {
        let dir = tempfile::tempdir().unwrap();
        temp_env::with_var("HOME", Some(dir.path().to_str().unwrap()), || {
            assert_eq!(
                path("dnd.toml"),
                Some(dir.path().join(".config/trollshell/dnd.toml"))
            );
        });
    }

    /// `file::path` and `xdg::Env::config_home` must agree on the same
    /// `$HOME`: for every value tried, either both resolve to the same
    /// directory or both refuse. Before #1009 this went red on the empty
    /// and relative cases — `xdg` had already learned (#985) to reject
    /// them, but `path()` still read `$HOME` on its own and accepted both,
    /// resolving into the process's working directory while `xdg` reported
    /// no config home at all.
    #[test]
    fn path_and_xdg_agree_on_the_same_home() {
        let dir = tempfile::tempdir().unwrap();
        let absolute = dir.path().to_str().unwrap().to_string();

        for home in ["", "relative-home", absolute.as_str()] {
            temp_env::with_vars(
                [("HOME", Some(home)), ("XDG_CONFIG_HOME", None::<&str>)],
                || {
                    let file_path = path("dnd.toml");
                    let xdg_path = Env::from_process()
                        .config_home()
                        .map(|dir| dir.join(crate::xdg::APP_DIR).join("dnd.toml"));
                    assert_eq!(
                        file_path, xdg_path,
                        "file::path and xdg::Env::config_home disagree for HOME={home:?}"
                    );
                },
            );
        }
    }

    /// Every name in `dir`, sorted — used to prove no temp file survives.
    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn writes_a_new_file_and_creates_its_parent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/deeper/dnd.toml");

        assert!(write_path("test", &path, "enabled = true\n"));

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "enabled = true\n",
            "the body should land verbatim"
        );
    }

    #[test]
    fn overwrites_an_existing_file_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("places.toml");
        // Longer than the replacement, so a truncate-then-write bug (or a
        // write-in-place one) would leave a tail behind.
        std::fs::write(&path, "x".repeat(4096)).unwrap();

        assert!(write_path("test", &path, "[[place]]\n"));

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[[place]]\n");
    }

    #[test]
    fn preserves_the_permissions_of_an_existing_target() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secretish.toml");
        std::fs::write(&path, "old\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        assert!(write_path("test", &path, "new\n"));

        assert_eq!(
            mode_of(&path),
            0o600,
            "a 0600 config must not come back world-readable"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new\n");

        // A mode the *umask* would eat. `OpenOptions::mode` is masked, so this
        // one only survives because of the explicit `fchmod` in `fill`; without
        // it the 0600 case above still passes (0600 clears no umask bits) and
        // the regression goes unnoticed under the usual 022.
        let group = dir.path().join("group-writable.toml");
        std::fs::write(&group, "old\n").unwrap();
        std::fs::set_permissions(&group, std::fs::Permissions::from_mode(0o664)).unwrap();

        assert!(write_path("test", &group, "new\n"));

        assert_eq!(
            mode_of(&group),
            0o664,
            "the umask must not narrow a carried-over mode"
        );
    }

    #[test]
    fn a_new_file_gets_the_same_mode_std_fs_write_would_have_given_it() {
        // The umask is the process's, so assert equivalence with the call this
        // replaced rather than hard-coding 0644.
        let dir = tempfile::tempdir().unwrap();
        let reference = dir.path().join("reference.toml");
        std::fs::write(&reference, "body\n").unwrap();
        let path = dir.path().join("fresh.toml");

        assert!(write_path("test", &path, "body\n"));

        assert_eq!(mode_of(&path), mode_of(&reference));
    }

    /// Both [`Durability`] arms produce the same file — they differ only in
    /// whether the *rename* is flushed, which no in-process test can observe.
    /// That is all this proves; it is not a guard against the `FsyncParent`
    /// branch going dead (inverting the `matches!` in [`write_atomic`], or
    /// swapping which variant `places::persist_to` passes, both leave this
    /// test green). The actual tripwire for that is
    /// `places::tests::persist_to_pins_the_fsync_parent_durability_choice`,
    /// via [`fsync_parent_attempts`].
    #[test]
    fn both_durability_choices_write_the_same_file() {
        let dir = tempfile::tempdir().unwrap();
        for (durability, name) in [
            (Durability::FileOnly, "toggle.toml"),
            (Durability::FsyncParent, "places.toml"),
        ] {
            let path = dir.path().join(name);
            write_atomic(&path, "body\n", durability).expect("writes");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "body\n");
        }
        assert_eq!(
            entries(dir.path()),
            vec!["places.toml".to_string(), "toggle.toml".to_string()],
            "neither arm may leave a temp file behind"
        );
    }

    #[test]
    fn leaves_no_temp_file_behind_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dnd.toml");

        assert!(write_path("test", &path, "enabled = true\n"));
        assert!(write_path("test", &path, "enabled = false\n"));

        assert_eq!(entries(dir.path()), vec!["dnd.toml".to_string()]);
    }

    #[test]
    fn leaves_no_temp_file_behind_when_the_rename_fails() {
        // A directory at the target path fails `rename(2)` with EISDIR for
        // every uid (root included), which makes this a deterministic failure
        // of the last step — the only step that runs with a temp file on disk.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wedged.toml");
        std::fs::create_dir(&path).unwrap();

        assert!(
            !write_path("test", &path, "body\n"),
            "a failed write must report false"
        );

        assert_eq!(
            entries(dir.path()),
            vec!["wedged.toml".to_string()],
            "the temp file must be cleaned up on the error path"
        );
    }

    #[test]
    fn reports_false_when_the_parent_cannot_be_created() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, "in the way\n").unwrap();

        assert!(!write_path("test", &blocker.join("dnd.toml"), "body\n"));

        assert_eq!(entries(dir.path()), vec!["not-a-dir".to_string()]);
    }

    #[test]
    fn writes_through_a_symlinked_target() {
        // A config symlinked into a dotfiles repo keeps working: we replace the
        // file the link points at, not the link.
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let realfile = real.join("places.toml");
        std::fs::write(&realfile, "old\n").unwrap();
        let link = dir.path().join("places.toml");
        std::os::unix::fs::symlink(&realfile, &link).unwrap();

        assert!(write_path("test", &link, "new\n"));

        assert!(
            std::fs::symlink_metadata(&link).unwrap().is_symlink(),
            "the symlink must survive the write"
        );
        assert_eq!(std::fs::read_to_string(&realfile).unwrap(), "new\n");
        assert_eq!(entries(&real), vec!["places.toml".to_string()]);
    }

    /// A "link first, populate later" dotfiles setup (stow/chezmoi): the link
    /// exists but its destination hasn't been created yet, so
    /// `canonicalize` fails on it (ENOENT on the final component). The write
    /// must still land through the link — not replace it with a regular file
    /// (#986). The non-dangling sibling is
    /// [`writes_through_a_symlinked_target`], where the real file already
    /// exists.
    #[test]
    fn writes_through_a_dangling_symlinked_target() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let realfile = real.join("places.toml");
        let link = dir.path().join("places.toml");
        std::os::unix::fs::symlink(&realfile, &link).unwrap();

        assert!(write_path("test", &link, "new\n"));

        assert!(
            std::fs::symlink_metadata(&link).unwrap().is_symlink(),
            "the symlink must survive the write, not be replaced by a regular file"
        );
        assert_eq!(std::fs::read_to_string(&realfile).unwrap(), "new\n");
        assert_eq!(entries(&real), vec!["places.toml".to_string()]);
    }

    /// The resolved target — not just the original `path` — must be
    /// discoverable from the returned error, so a caller's `error = %e` log
    /// (`write_path`) or a stringified error (`PlacesError::Write`) tells the
    /// operator which directory is actually missing, rather than a bare "No
    /// such file or directory" naming nothing. Review follow-up on #986: a
    /// dangling symlink whose target's own directory doesn't exist either.
    #[test]
    fn a_missing_target_directory_names_the_resolved_path_in_the_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing_dir = dir.path().join("nope");
        let target = missing_dir.join("places.toml");
        let link = dir.path().join("places.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let err = write_atomic(&link, "new\n", Durability::FileOnly).unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(
            err.to_string().contains(&target.display().to_string()),
            "error must name the resolved target, not just the original link: {err}"
        );
        assert!(
            std::fs::symlink_metadata(&link).unwrap().is_symlink(),
            "the link must survive a failed write"
        );
        assert!(
            !missing_dir.exists(),
            "the missing directory must not be created"
        );
    }

    /// The one error raised before `target` is resolved — `create_dir_all`
    /// on the original `path`'s parent — must name that parent too, or a
    /// `path` component that already exists as a plain file reaches the
    /// caller as a pathless `File exists (os error 17)` (#1009's second
    /// item). The kind is preserved exactly as the later errors preserve it.
    #[test]
    fn a_plain_file_in_the_way_of_the_parent_names_that_parent_in_the_error() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("cfg");
        std::fs::write(&plain, "not a directory\n").unwrap();
        let path = plain.join("places.toml");

        let err = write_atomic(&path, "new\n", Durability::FileOnly).unwrap_err();

        let raw = std::fs::create_dir_all(&plain).unwrap_err();
        assert_eq!(
            err.kind(),
            raw.kind(),
            "the kind must be the OS's own: {err}"
        );
        assert!(
            err.to_string().contains(&plain.display().to_string()),
            "error must name the parent that is in the way: {err}"
        );
        assert_eq!(
            std::fs::read_to_string(&plain).unwrap(),
            "not a directory\n",
            "the plain file must survive untouched"
        );
    }

    /// A symlink that points at itself resolves to nothing no matter how
    /// many hops are allowed. The pre-#986 fallback of writing through
    /// `path` itself would `rename(2)` a regular file over exactly this
    /// link — review follow-up on #986's HIGH finding.
    #[test]
    fn a_self_referential_symlink_is_rejected_not_written_through() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("a");
        std::os::unix::fs::symlink(&link, &link).unwrap();

        assert!(!write_path("test", &link, "new\n"));

        assert!(
            std::fs::symlink_metadata(&link).unwrap().is_symlink(),
            "a self-referential symlink must not be replaced by a regular file"
        );
    }

    /// The two-link sibling of the self-loop above: `a -> b -> a`. Neither
    /// link may be touched by a rejected write.
    #[test]
    fn a_two_link_symlink_cycle_is_rejected_not_written_through() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::os::unix::fs::symlink(&b, &a).unwrap();
        std::os::unix::fs::symlink(&a, &b).unwrap();

        assert!(!write_path("test", &a, "new\n"));

        assert!(
            std::fs::symlink_metadata(&a).unwrap().is_symlink(),
            "the first link of a rejected cycle must survive"
        );
        assert!(
            std::fs::symlink_metadata(&b).unwrap().is_symlink(),
            "the second link of a rejected cycle must survive"
        );
    }

    /// Builds a chain of `n` symlinks under `dir`:
    /// `link0 -> link1 -> … -> link{n-1} -> final.toml`, where `final.toml`
    /// is never created. Returns `(link0, final.toml)` — the entry point a
    /// caller writes through, and the path a successful resolve must land
    /// on.
    fn symlink_chain(dir: &Path, n: u32) -> (PathBuf, PathBuf) {
        let final_target = dir.join("final.toml");
        let mut next = final_target.clone();
        for i in (0..n).rev() {
            let link = dir.join(format!("link{i}"));
            std::os::unix::fs::symlink(&next, &link).unwrap();
            next = link;
        }
        (next, final_target)
    }

    /// A chain exactly [`MAX_SYMLINK_HOPS`] long is still an ordinary,
    /// resolvable symlink chain — the same length the kernel's own path
    /// resolution would follow — so it must succeed, not be mistaken for a
    /// cycle. Review follow-up on #986's off-by-one MEDIUM finding: the
    /// lower boundary of the fixed bound.
    #[test]
    fn a_symlink_chain_at_exactly_the_hop_bound_resolves() {
        let dir = tempfile::tempdir().unwrap();
        let (entry, target) = symlink_chain(dir.path(), MAX_SYMLINK_HOPS);

        assert!(write_path("test", &entry, "new\n"));

        assert!(
            std::fs::symlink_metadata(&entry).unwrap().is_symlink(),
            "the entry link must survive a successful write-through"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new\n");
    }

    /// One hop past the bound must be rejected — the upper boundary the
    /// off-by-one fix exists to place correctly, the sibling of the previous
    /// test.
    #[test]
    fn a_symlink_chain_one_past_the_hop_bound_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let (entry, target) = symlink_chain(dir.path(), MAX_SYMLINK_HOPS + 1);

        assert!(!write_path("test", &entry, "new\n"));

        assert!(
            std::fs::symlink_metadata(&entry).unwrap().is_symlink(),
            "the entry link must survive a rejected write"
        );
        assert!(
            !target.exists(),
            "an over-long chain must not create the target"
        );
    }

    /// A reader hammering the file while a writer replaces it must never see a
    /// partial body.
    ///
    /// This is the property the issue is actually about, and it can't be made
    /// deterministic without injecting a scheduling hook into the write path.
    /// It is however not *flaky*: an unlucky interleaving makes the test prove
    /// less, never fail — with the old `std::fs::write` it caught the tear on
    /// the first run, and with `rename(2)` there is no interleaving that can
    /// fail it. No sleeps, so it costs a few milliseconds either way.
    #[test]
    fn a_reader_never_observes_a_partial_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.toml");
        let bodies = [
            Arc::new("a".repeat(32 * 1024)),
            Arc::new("b".repeat(32 * 1024)),
        ];
        assert!(write_path("test", &path, &bodies[0]));

        let stop = Arc::new(AtomicBool::new(false));
        let reader = std::thread::spawn({
            let (path, stop) = (path.clone(), Arc::clone(&stop));
            let (a, b) = (Arc::clone(&bodies[0]), Arc::clone(&bodies[1]));
            move || {
                while !stop.load(Ordering::Relaxed) {
                    if let Ok(seen) = std::fs::read_to_string(&path) {
                        assert!(
                            seen == *a || seen == *b,
                            "torn read: {} bytes of {}",
                            seen.len(),
                            a.len()
                        );
                    }
                    std::thread::yield_now();
                }
            }
        });

        for i in 0..24_usize {
            assert!(write_path("test", &path, &bodies[i % 2]));
        }
        stop.store(true, Ordering::Relaxed);
        reader
            .join()
            .expect("reader thread saw a partial file (its panic has the byte count)");

        assert_eq!(entries(dir.path()), vec!["big.toml".to_string()]);
    }

    /// Two writers racing on one path: last writer wins, but neither may see
    /// the other's bytes, and neither may leave a temp file behind.
    #[test]
    fn concurrent_writers_do_not_corrupt_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("contended.toml");
        let bodies = [Arc::new("a".repeat(8192)), Arc::new("b".repeat(8192))];

        let writers: Vec<_> = bodies
            .iter()
            .map(|body| {
                let (path, body) = (path.clone(), Arc::clone(body));
                std::thread::spawn(move || {
                    for _ in 0..24 {
                        assert!(write_path("test", &path, &body));
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().unwrap();
        }

        let final_body = std::fs::read_to_string(&path).unwrap();
        assert!(
            final_body == *bodies[0] || final_body == *bodies[1],
            "the survivor must be one whole body, not a blend"
        );
        assert_eq!(entries(dir.path()), vec!["contended.toml".to_string()]);
    }
}
