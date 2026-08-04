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
//! Writes are also **atomic** (#733): [`write`] renders into a temp file beside
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
//! [`write`] and [`write_path`] are the `$HOME`-resolving, `warn!`-logging,
//! `bool`-returning wrapper the UI-state services want; [`crate::places`] calls
//! the core directly because it needs the `io::Error` to build a
//! `PlacesError::Write`. The one axis on which those two callers genuinely
//! differ is [`Durability`], which is therefore a stated parameter rather than
//! a house default — see that type for which caller picks what, and why.

use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Directory (relative to `$HOME`) all trollshell config files live under.
const CONFIG_SUBDIR: &str = ".config/trollshell";

/// Distinguishes the temp files of two writes that overlap in time.
///
/// Combined with the pid it is unique across the machine: two threads of this
/// process take different tickets, and no other live process shares our pid.
/// Last writer wins on the target, which is the pre-existing contract; what
/// this prevents is two writers sharing one temp file and interleaving into it.
static TMP_TICKET: AtomicU64 = AtomicU64::new(0);

/// Whether [`write_atomic`] also `fsync`s the parent **directory** after the
/// `rename(2)`.
///
/// The *file's* own `fsync` is unconditional (see [`fill`]) — without it the
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
pub(crate) enum Durability {
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
    /// For the click-driven `~/.config/trollshell/*` toggles behind [`write`]:
    /// they are rewritten constantly, several straight out of a click handler,
    /// and they are explicitly a convenience for the *next* launch — the
    /// in-memory `Mutable` is the source of truth for the running process. A
    /// power cut that costs the last toggle leaves the previous config intact,
    /// so the second journal commit per click isn't worth it.
    FileOnly,
}

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
/// Atomic, symlink-safe and permission-preserving — see [`write_atomic`], which
/// does the actual work. These files take [`Durability::FileOnly`]: they are
/// click-driven toggles, and the rationale for not paying a directory `fsync`
/// per click is on that variant.
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
    write_path(service, &path, body)
}

/// [`write`] against an already-resolved absolute path — the whole of `write`
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

/// Atomically replace `path`'s contents with `body`, creating the parent dir.
///
/// The workspace's single copy of tmp + `fsync` + `rename(2)` + cleanup (#739);
/// every persisted config file goes through here, whether via [`write`] or via
/// `places`' own writer. No logging and no service scope — the caller decides
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
/// doesn't exist yet can't be canonicalised, and needs no resolving.
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
/// leaves the target untouched.
pub(crate) fn write_atomic(path: &Path, body: &str, durability: Durability) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Resolve symlinks: `std::fs::write` wrote *through* a symlinked target, so
    // follow it rather than replacing the link with a regular file. It also
    // keeps the temp file next to the real file — `rename(2)` is only atomic
    // within one filesystem. A target that doesn't exist yet can't be
    // canonicalised, and needs no resolving.
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let dir = target
        .parent()
        .ok_or_else(|| std::io::Error::other("config path has no parent directory"))?;
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
        // own error is what the caller needs to see.
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    if matches!(durability, Durability::FsyncParent) {
        let _ = std::fs::File::open(dir).and_then(|d| d.sync_all());
    }
    Ok(())
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
/// Best-effort, like [`write`]: a missing file is success (nothing to do); a
/// `$HOME`-unset or non-`NotFound` I/O error logs a `warn!` scoped to
/// `service`. Callers use it to return a persisted UI-state toggle to its
/// zero-state (e.g. the wallpaper picker's "Clear" clearing the render files).
pub(crate) fn remove(service: &str, file: &str) {
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
    /// The `FsyncParent` arm is what `places::persist_to` picks (#739); this is
    /// the local proof it isn't a dead branch. Its real coverage is the
    /// `persist_to` suite in `places`.
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
