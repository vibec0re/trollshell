//! The durable grant store: `grants.toml` under the XDG state dir.
//!
//! A grant is the policy half of the design (issue #487): it is **durable**
//! (survives a broker/shell restart), keyed `(agent × datasource × scope)`, and
//! carries a [`Decision`]. Tokens — the ephemeral half — live only in memory
//! ([`crate::tokens`]).
//!
//! Phase 1a only ever writes `always`/`deny` decisions (the interactive
//! `once`/`session` prompting shipped in phase 1b (#514), but those decisions
//! are ephemeral and never reach this durable store), and `scope` is always
//! [`SCOPE_ALL`] — the field exists so a finer scope (a specific station, a
//! read-vs-subscribe split) is additive later without a schema break.
//!
//! ```toml
//! # ~/.local/state/hytte-infobroker/grants.toml
//! [[grant]]
//! agent = "claude"
//! datasource = "departures"
//! scope = "*"
//! decision = "always"
//! ```

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{UnboundedSender, error::SendError, unbounded_channel};

/// The wildcard scope: the whole datasource. The only scope phase 1a mints.
pub const SCOPE_ALL: &str = "*";

/// A grant's decision. Phase 1a persists only these two; the interactive
/// `once`/`session` decisions are a 1b concern and never reach the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Decision {
    /// Always allow — auth mints silently and the matching `get` is served.
    Always,
    /// Always deny — a standing "no" that blocks even a re-ask.
    Deny,
}

impl Decision {
    /// The wire/CLI string form (`"always"` / `"deny"`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Decision::Always => "always",
            Decision::Deny => "deny",
        }
    }
}

/// One durable grant row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    /// The agent identity (the `--agent` name the CLI authed with).
    pub agent: String,
    /// The datasource the grant covers (e.g. `"departures"`).
    pub datasource: String,
    /// The scope within the datasource; [`SCOPE_ALL`] in phase 1a.
    #[serde(default = "default_scope")]
    pub scope: String,
    /// Allow or deny.
    pub decision: Decision,
}

fn default_scope() -> String {
    SCOPE_ALL.to_owned()
}

impl Grant {
    /// An `always` grant for `(agent, datasource)` at the wildcard scope.
    #[must_use]
    pub fn always(agent: impl Into<String>, datasource: impl Into<String>) -> Self {
        Self {
            agent: agent.into(),
            datasource: datasource.into(),
            scope: SCOPE_ALL.to_owned(),
            decision: Decision::Always,
        }
    }
}

/// The TOML envelope: a table of `[[grant]]` arrays.
#[derive(Debug, Default, Serialize, Deserialize)]
struct GrantsFile {
    #[serde(default)]
    grant: Vec<Grant>,
}

/// One queued snapshot for a store's single writer task: the rendered
/// `grants.toml` body plus the blocking step that lands it (production's
/// [`write_atomic`], or a test's injected writer — see
/// [`save_with`](GrantStore::save_with)).
type WriteJob = (
    String,
    Box<dyn FnOnce(&Path, &str) -> std::io::Result<()> + Send>,
);

/// The in-memory grant set plus the file it persists to. Construct via
/// [`GrantStore::load`] (disk) or [`GrantStore::from_grants`] (tests).
#[derive(Debug)]
pub struct GrantStore {
    path: Option<PathBuf>,
    grants: Vec<Grant>,
    /// The store's **single writer** lane (#1074 review M1), created on the
    /// first [`save`](GrantStore::save) that has a tokio runtime to spawn it
    /// on. Every snapshot goes through this one channel so writes land in
    /// submission order — see `save`'s doc. `OnceLock` rather than a field
    /// set in the constructors because `save` takes `&self` and because a
    /// store is routinely built off the runtime (`load_grants` runs inside
    /// `spawn_blocking`), where there is no handle to spawn from yet.
    writer: OnceLock<UnboundedSender<WriteJob>>,
}

impl GrantStore {
    /// Load the store from `path`, treating a missing file as an empty store
    /// (the first-run case — no grants yet). A present-but-malformed file is an
    /// error rather than silently dropped, so a typo doesn't quietly grant/deny.
    ///
    /// Also sweeps stale temp siblings left by a crash mid-write (#1074 review
    /// M5) — see [`sweep_stale_tmp`].
    ///
    /// # Errors
    /// If the file exists but can't be read or parsed as the grant schema.
    pub fn load(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        let grants = match std::fs::read_to_string(&path) {
            Ok(text) => parse_grants(&text)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(format!("reading {}: {e}", path.display())),
        };
        sweep_stale_tmp(&path);
        Ok(Self {
            path: Some(path),
            grants,
            writer: OnceLock::new(),
        })
    }

    /// An in-memory store with no backing file — for tests and for the rare
    /// no-`HOME` runtime where [`crate::paths::grants_path`] yields `None`.
    #[must_use]
    pub fn from_grants(grants: Vec<Grant>) -> Self {
        Self {
            path: None,
            grants,
            writer: OnceLock::new(),
        }
    }

    /// All grants, in file order.
    #[must_use]
    pub fn grants(&self) -> &[Grant] {
        &self.grants
    }

    /// The standing decision for `(agent, datasource)`, or `None` if no grant
    /// covers it. Scope-agnostic in phase 1a (only [`SCOPE_ALL`] exists).
    #[must_use]
    pub fn decision_for(&self, agent: &str, datasource: &str) -> Option<Decision> {
        self.grants
            .iter()
            .find(|g| g.agent == agent && g.datasource == datasource)
            .map(|g| g.decision)
    }

    /// Whether `agent` has at least one `always` grant (any datasource) — the
    /// gate `auth` mints a token on.
    #[must_use]
    pub fn has_any_always(&self, agent: &str) -> bool {
        self.grants
            .iter()
            .any(|g| g.agent == agent && g.decision == Decision::Always)
    }

    /// Whether `agent` has at least one `deny` grant (any datasource) — a
    /// standing "no" that `auth` refuses **without** re-prompting (#487 phase
    /// 1b): a deliberate `Deny` decision is remembered, so the agent is denied
    /// silently rather than knocking again.
    #[must_use]
    pub fn has_any_deny(&self, agent: &str) -> bool {
        self.grants
            .iter()
            .any(|g| g.agent == agent && g.decision == Decision::Deny)
    }

    /// Add (or upgrade an existing row to) an `always` grant for
    /// `(agent, datasource)` and persist (off the caller's thread — see
    /// [`save`](GrantStore::save)). Idempotent: a matching row is updated in
    /// place rather than duplicated.
    pub fn grant_always(&mut self, agent: &str, datasource: &str) {
        if let Some(g) = self
            .grants
            .iter_mut()
            .find(|g| g.agent == agent && g.datasource == datasource)
        {
            g.decision = Decision::Always;
            SCOPE_ALL.clone_into(&mut g.scope);
        } else {
            self.grants.push(Grant::always(agent, datasource));
        }
        self.save();
    }

    /// Add (or downgrade an existing row to) a `deny` grant for
    /// `(agent, datasource)` and persist (off the caller's thread — see
    /// [`save`](GrantStore::save)) — the durable half of an `AllowAlways`
    /// decision's opposite: a deliberate `Deny` consent (#487 phase 1b). Idempotent
    /// (a matching row is updated in place), mirroring [`grant_always`](GrantStore::grant_always).
    pub fn grant_deny(&mut self, agent: &str, datasource: &str) {
        if let Some(g) = self
            .grants
            .iter_mut()
            .find(|g| g.agent == agent && g.datasource == datasource)
        {
            g.decision = Decision::Deny;
            SCOPE_ALL.clone_into(&mut g.scope);
        } else {
            self.grants.push(Grant {
                agent: agent.to_owned(),
                datasource: datasource.to_owned(),
                scope: SCOPE_ALL.to_owned(),
                decision: Decision::Deny,
            });
        }
        self.save();
    }

    /// Remove the grant for `(agent, datasource)` and persist (off the
    /// caller's thread — see [`save`](GrantStore::save)). Returns whether a
    /// row was actually removed (so the caller only kills tokens on a real
    /// revoke).
    pub fn revoke(&mut self, agent: &str, datasource: &str) -> bool {
        let before = self.grants.len();
        self.grants
            .retain(|g| !(g.agent == agent && g.datasource == datasource));
        let removed = self.grants.len() != before;
        if removed {
            self.save();
        }
        removed
    }

    /// Persist the current grant set to the backing file, off the runtime
    /// thread and atomically (#1065 — deferred from #1064/#1059).
    ///
    /// Serializes the grant rows right here, under the caller's `&mut self`
    /// borrow — a `to_toml` call, cheap even for a large table, and the only
    /// part of this that touches `self`. The rendered bytes are then queued on
    /// the store's **single writer lane** (below), whose task does the actual
    /// I/O off this thread: `mkdir -p` the state dir (tightened to `0700`),
    /// write to a sibling `.<file>.<pid>.<ticket>.tmp` **at the target's own
    /// mode**, `fsync` it, then `rename(2)` over the target — the same
    /// write-then-rename shape `hytte_config::file::write_atomic` uses for
    /// `places.toml`/the `~/.config/trollshell/*` writer. This crate doesn't
    /// otherwise depend on `hytte-config` (and doesn't gain that dependency
    /// here), so [`write_atomic`] is the sequence copied rather than the
    /// function imported. It diverges from that helper in exactly three
    /// stated places, and no others:
    ///
    /// 1. **no symlink-following** — `grants.toml` is state, not a
    ///    hand-edited dotfile that a user might symlink into a dotfile repo;
    /// 2. **no parent-directory `fsync`** — which is precisely that helper's
    ///    `Durability::FileOnly`. The file's *own* `fsync` is kept, because
    ///    without it the rename can be durable while the data is not,
    ///    resurrecting a zero-length `grants.toml` — which *parses*, as zero
    ///    grants (`empty_body_is_an_empty_store`), i.e. every grant silently
    ///    forgotten;
    /// 3. **a `0600` first-run default** where that helper takes the umask.
    ///    Mode *preservation* is mirrored, not skipped (#1074 review M6): a
    ///    `rename(2)` carries the temp's mode onto the target, so a temp born
    ///    at the umask would silently undo a `chmod 600` on every save. Only
    ///    the mode of a file that doesn't exist yet differs, and it differs
    ///    tighter — see [`target_mode`].
    ///
    /// Atomicity closes PR #1064's review finding F2: since #1059 moved the
    /// session-start grant *load* to `spawn_blocking`, it is genuinely
    /// concurrent with a `save` for the first time in one process (they used
    /// to share the single runtime thread, so they were mutually exclusive by
    /// construction). A reader now always sees either the whole old file or
    /// the whole new one — never a truncated write in progress.
    ///
    /// # One writer, in submission order (#1074 review M1)
    ///
    /// `save` never blocks and the `select!` loop that reaches it through
    /// `apply_cmd`/`apply_consent` never awaits the write — it returns as
    /// soon as the snapshot is queued. The first version of this change
    /// queued each snapshot as its own **detached** `spawn_blocking`, which
    /// let two writes complete out of order: two `Cmd`s already buffered on
    /// the command lane (an Allow then a Revoke) are applied microseconds
    /// apart with no yielding await between them, and the blocking pool runs
    /// their writes on different threads. Measured on the pre-fix tree,
    /// through the public API on a 1-row table, the revoked grant was still
    /// on disk 16 times in 100 — and *stayed* there, since nothing re-writes
    /// the file until the next human grant change. That fails **open** (a
    /// valid file with the wrong policy: the revoked grant returns at the
    /// next broker restart), unlike the torn write this change closes, which
    /// fails closed.
    ///
    /// So the snapshots go through one [`tokio::sync::mpsc`] channel drained
    /// by exactly one task per store: it awaits each write's own
    /// `spawn_blocking` before taking the next job, so a newer snapshot can
    /// never be overtaken by an older one. Because every job carries the
    /// *whole* grant set, a burst is collapsed to its last member
    /// (latest-wins) rather than written N times. The drain loop is an
    /// ordinary `spawn`ed task rather than a `spawn_blocking` one parked in
    /// `blocking_recv`: a blocking-pool thread parked on a channel is not
    /// woken by runtime shutdown, so dropping a runtime while any store is
    /// still alive would hang forever in `Runtime::drop` (measured — see the
    /// PR).
    ///
    /// **The scope of that guarantee is one store handle, for saves issued
    /// with a runtime in context** (#1074 review M7). The lane is a field of
    /// *this* `GrantStore`, so two stores loaded over the same path each get
    /// their own, and the no-runtime fallback below writes outside the lane
    /// altogether — either way two writes to one file can complete out of
    /// order, exactly as they did before the lane existed. Neither is
    /// reachable in-tree: every mutator runs on the broker's `select!` loop
    /// (`Handle::try_current()` is `Ok` there, and `Ok` inside
    /// `spawn_blocking` too), the CLI never mutates, and session N+1's store
    /// cannot apply a `Cmd` until session N's loop has exited. A second
    /// mutator on another thread, or a second live store, would need its own
    /// answer rather than inheriting this one.
    ///
    /// # Without a tokio runtime (#1074 review M4)
    ///
    /// `grant_always`/`grant_deny`/`revoke` are `pub`, and the same library
    /// is linked by the runtime-less `hytte-infobroker` CLI, so `save` must
    /// not require a reactor. With no runtime in context it performs the
    /// pre-#1065 **inline** write on the calling thread — the same behaviour
    /// the CLI would have got before this change, and the same fallback used
    /// if the writer task is gone (a shutting-down runtime), where dropping
    /// the write would be worse than blocking briefly for it. Both of those
    /// writes bypass the lane, per the scope note above.
    ///
    /// A store with no backing file is a no-op (test / no-`HOME`). A failure —
    /// encoding, creating the state dir, the write, or the rename — is logged
    /// from wherever it happens: encoding is synchronous here, so it logs and
    /// returns without queueing; the rest is logged by the writer task, since
    /// by the time it can fail `save` has already returned to its caller.
    fn save(&self) {
        self.save_with(write_atomic);
    }

    /// [`save`](GrantStore::save), with the blocking write step supplied by
    /// the caller — the test seam for the property `save` exists to buy:
    /// "a slow synchronous write does not stall a concurrent timer on this
    /// runtime". Production is just `save`, i.e. `self.save_with(write_atomic)`;
    /// `grants::tests::save_offloads_a_slow_writer_without_delaying_a_concurrent_timer`
    /// injects one that blocks for two seconds instead — same shape as
    /// `broker::serve_with_grant_loader`'s injected loader (#1059's test
    /// seam), one level down. Kept `fn(&self)`-private: nothing outside this
    /// module needs it, so unlike `serve_with_grant_loader` it never has to
    /// be `pub`.
    fn save_with<W>(&self, writer: W)
    where
        W: FnOnce(&Path, &str) -> std::io::Result<()> + Send + 'static,
    {
        let Some(path) = self.path.clone() else {
            return;
        };
        let text = match to_toml(&self.grants) {
            Ok(text) => text,
            Err(e) => {
                log(&format!("encoding grants.toml: {e}"));
                return;
            }
        };
        // No reactor (the CLI links this library; so do plain `#[test]`s) →
        // the pre-#1065 inline write. See `save`'s doc, "Without a tokio
        // runtime".
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            write_inline(&path, &text, writer);
            return;
        };
        let queue = self
            .writer
            .get_or_init(|| spawn_writer(&handle, path.clone()));
        if let Err(SendError((text, writer))) = queue.send((text, Box::new(writer))) {
            // The drain task is gone — its runtime is shutting down. Losing a
            // grant change is worse than blocking this thread for one write.
            write_inline(&path, &text, writer);
        }
    }
}

/// Run one write step on the calling thread, logging a failure the way the
/// writer task does. The no-runtime / dead-writer fallback for
/// [`GrantStore::save_with`].
///
/// **This write does not participate in the lane's ordering** (#1074 review
/// M7): it is issued straight to the filesystem, concurrently with anything
/// [`spawn_writer`]'s task already has in flight. Only reachable with no
/// runtime in context (so: not from any in-tree mutator, all of which run on
/// the broker's runtime) or once that task is gone.
fn write_inline<W>(path: &Path, text: &str, writer: W)
where
    W: FnOnce(&Path, &str) -> std::io::Result<()>,
{
    if let Err(e) = writer(path, text) {
        log(&format!("writing {}: {e}", path.display()));
    }
}

/// Spawn a store's one writer task and hand back the lane that feeds it.
///
/// Strict FIFO by construction: the loop `await`s each job's own
/// [`tokio::task::spawn_blocking`] before it takes the next, so two snapshots
/// can never be in flight at once and the newest queued one is always the
/// last written. Jobs already waiting when one is picked up are collapsed to
/// the last of them — each carries the complete grant set, so the earlier
/// ones are redundant, and a burst of panel clicks costs one write rather
/// than N.
///
/// The loop ends when the store (and with it the sender) is dropped, so a
/// store's task never outlives it — and a snapshot already queued when the
/// sender goes is still written, because `recv` drains the buffer before it
/// reports the channel closed. Session teardown therefore loses nothing.
///
/// **Runtime shutdown is the other side of the `spawn`-not-`spawn_blocking`
/// trade** (#1074 review M8): dropping the runtime *cancels* this task, so a
/// snapshot still sitting in the channel is silently dropped — measured, a
/// `Runtime::drop` immediately after one `grant_always` returned in 39.5 µs
/// with `grants.toml` never created. That is the price of not hanging (a
/// blocking-pool thread parked in `blocking_recv` is never woken by
/// shutdown, so the alternative deadlocks). Production never pays it:
/// `hytte_plugin::run<P>() -> !` `block_on`s a diverging loop, so the SDK
/// runtime is not dropped at all — the reachable half of this class is
/// `SIGTERM`, tracked with the SDK exit hook in #1079.
fn spawn_writer(handle: &tokio::runtime::Handle, path: PathBuf) -> UnboundedSender<WriteJob> {
    let (tx, mut rx) = unbounded_channel::<WriteJob>();
    handle.spawn(async move {
        while let Some(mut job) = rx.recv().await {
            // Latest-wins coalescing over whatever is already queued.
            while let Ok(newer) = rx.try_recv() {
                job = newer;
            }
            let (text, writer) = job;
            let write_path = path.clone();
            match tokio::task::spawn_blocking(move || writer(&write_path, &text)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => log(&format!("writing {}: {e}", path.display())),
                Err(e) => log(&format!(
                    "writing {}: write task failed: {e}",
                    path.display()
                )),
            }
        }
    });
    tx
}

/// `[infobroker]`-prefixed eprintln, matching `broker::tracing_eprintln`'s
/// format. Duplicated rather than shared across the module boundary: `grants`
/// is the lower-level, `broker`-independent module (it's the one `broker`
/// imports from), so reaching back up to `crate::broker` for one log line
/// would invert that.
fn log(msg: &str) {
    eprintln!("[infobroker] {msg}");
}

/// Distinguishes the temp files of two [`GrantStore::save`] calls that
/// overlap in time (e.g. an Allow immediately followed by a Revoke, each
/// spawning its own detached write) — mirrors `hytte_config::file`'s
/// `TMP_TICKET`. Without a distinct name per write, two in-flight writers to
/// the same target would share one temp file and interleave into it: two
/// open file descriptors to one inode isn't a "last write wins" race, it's
/// byte-level corruption of the tmp file itself, before `rename` even runs.
static TMP_TICKET: AtomicU64 = AtomicU64::new(0);

/// Atomically replace `path`'s contents with `text`: create its parent
/// directory (tightened to `0700`, same as [`GrantStore::load`]'s caller
/// expects), write **and `fsync`** a sibling temp file **at the target's own
/// mode**, then `rename(2)` over the target.
///
/// Copies `hytte_config::file::write_atomic`'s core sequence — a temp file in
/// the target's own directory, so the rename stays on one filesystem and is
/// genuinely atomic, opened at the mode the target already carries so the
/// rename cannot change it — without linking that crate. The full list of
/// what it deliberately does *not* copy is on [`GrantStore::save`]'s doc, and
/// it is three items: no symlink-following (`grants.toml` is state, not a
/// hand-edited dotfile), no optional parent-directory `fsync` (i.e. precisely
/// that helper's `Durability::FileOnly`), and a `0600` rather than umask
/// default for a file that does not exist yet ([`target_mode`]).
///
/// The **file's own `fsync` is not optional** and is why this writes through
/// `OpenOptions` instead of `std::fs::write` (#1074 review M2): without it the
/// `rename` can be durable across a power cut while the *data* is not, leaving
/// a zero-length `grants.toml` — which parses, as zero grants
/// (`empty_body_is_an_empty_store`), silently forgetting every grant. That is
/// the same outcome
/// `write_atomic_never_exposes_a_torn_file_to_a_concurrent_reader` exists to
/// prevent, reached by a crash instead of by a concurrent read.
///
/// Synchronous by design: every caller runs it from
/// [`tokio::task::spawn_blocking`], never inline on the runtime thread —
/// except [`GrantStore::save_with`]'s deliberate no-runtime fallback.
fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let dir = path.parent().ok_or_else(|| {
        std::io::Error::other(format!(
            "{}: grants path has no parent directory",
            path.display()
        ))
    })?;
    std::fs::create_dir_all(dir)
        .map_err(|e| std::io::Error::new(e.kind(), format!("{}: {e}", dir.display())))?;
    tighten_dir(dir);
    let ticket = TMP_TICKET.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("grants.toml");
    let tmp = dir.join(format!(".{name}.{}.{ticket}.tmp", std::process::id()));
    if let Err(e) = fill_tmp(&tmp, text, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// The mode `grants.toml` should carry after the rename: the one it already
/// has, or `0600` when there is no file yet (#1074 review M6).
///
/// A `rename(2)` carries the *temp* file's mode onto the target, so a temp
/// born at the process umask (0644 under the usual 0022) silently undoes a
/// `chmod 600` on every save — a regression against `main`, whose
/// `std::fs::write` was a `create+truncate` open that left an existing file's
/// mode alone. `hytte_config::file::write_atomic` preserves the target's mode
/// for exactly this reason; the only place this differs from that helper is
/// the **first-run** default, which is `0600` rather than the umask, because
/// `grants.toml` is the broker's policy file rather than a hand-edited
/// dotfile. The 0700 state dir (see [`tighten_dir`]) is defence in depth on
/// top, not a substitute: a mode leaks past the directory through backups,
/// `rsync -a`, tarballs, and an `$XDG_STATE_HOME` pointed somewhere shared.
#[cfg(unix)]
fn target_mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).map_or(0o600, |m| m.permissions().mode() & 0o7777)
}

/// Write `text` into a freshly-created temp file for `target` and `fsync` it —
/// the durability half of [`write_atomic`], mirroring `hytte_config::file`'s
/// `fill`. See that function's doc for why the `sync_all` is load-bearing.
///
/// `target`'s mode ([`target_mode`]) is applied at `open` **and** re-asserted
/// on the fd, the same belt-and-braces `fill` uses: `OpenOptions::mode` only
/// takes effect when `open` actually creates the file, so it is silently
/// ignored if a temp file from a crashed earlier run happens to be sitting at
/// this name — which is reachable here precisely because a crash mid-write is
/// what [`sweep_stale_tmp`] exists to clean up after.
fn fill_tmp(tmp: &Path, text: &str, target: &Path) -> std::io::Result<()> {
    #[cfg(not(unix))]
    let _ = target;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    let mode = target_mode(target);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(mode);
    }
    let mut file = opts.open(tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    }
    file.write_all(text.as_bytes())?;
    file.sync_all()
}

/// How long a sibling temp file must have sat untouched before
/// [`sweep_stale_tmp`] treats it as crash litter rather than a live write.
///
/// A real [`write_atomic`] holds its temp file for one write + one `fsync` —
/// sub-millisecond in practice. A minute is several orders of magnitude of
/// headroom, which matters because the CLI links this library too: a
/// `hytte-infobroker` invocation calling [`GrantStore::load`] would otherwise
/// be able to delete the temp file of a write the broker process is in the
/// middle of.
///
/// Headroom, not a guarantee: a write whose `write_all`+`fsync` is stuck on a
/// wedged disk past this bound can still have its temp swept by a concurrent
/// `load`, after which the `rename` fails `ENOENT` and that one write is
/// logged and lost. Accepted — the in-memory store is still the session's
/// source of truth, and a disk that slow has already broken more than this.
///
/// Clock skew can only make the sweep *more* conservative: `SystemTime::
/// elapsed` returns `Err` for a future mtime, and [`sweep_stale_tmp`]'s
/// `is_ok_and` treats that as not-stale.
const STALE_TMP_AGE: Duration = Duration::from_mins(1);

/// Delete `path`'s stale `.<file>.<pid>.<ticket>.tmp` siblings (#1074 review
/// M5).
///
/// `hytte_plugin::run() -> !` never returns and neither the SDK nor this crate
/// installs a signal handler, so a `systemctl stop` landing inside a write
/// window leaves the temp file behind with nothing to clean it up — one
/// orphan per unlucky stop, accumulating in the state dir forever. Sweeping at
/// store open is the cheap half of the answer; the *lost write* half is an
/// SDK-wide question — every plugin that persists state has the same window —
/// tracked in #1079 rather than bolted into this one crate.
///
/// Only files this module could itself have written are candidates: the name
/// must match `.<file>.<pid>.<ticket>.tmp` **exactly**, both middle fields
/// parsed as integers (#1074 review M9). A prefix+suffix match alone would
/// also delete something like `.grants.toml.backup-before-i-edited-it.tmp`,
/// and then claim in the log that a crash left it.
///
/// Best-effort throughout: a missing directory, an unreadable entry or a
/// failed unlink is not a reason to fail a load.
fn sweep_stale_tmp(path: &Path) {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str())) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let entry_name = entry.file_name();
        let Some(entry_name) = entry_name.to_str() else {
            continue;
        };
        if !is_own_tmp_name(entry_name, name) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .is_ok_and(|m| m.elapsed().is_ok_and(|age| age > STALE_TMP_AGE));
        if stale && std::fs::remove_file(entry.path()).is_ok() {
            log(&format!(
                "swept stale temp file {} (left by a crash mid-write)",
                entry.path().display()
            ));
        }
    }
}

/// Whether `entry` is a name [`write_atomic`] could have minted for `target`:
/// exactly `.<target>.<pid>.<ticket>.tmp`, with both middle fields parsing as
/// integers. Split rather than regex'd — the crate has no regex dependency
/// and this is the whole grammar.
fn is_own_tmp_name(entry: &str, target: &str) -> bool {
    let Some(rest) = entry
        .strip_prefix('.')
        .and_then(|r| r.strip_prefix(target))
        .and_then(|r| r.strip_prefix('.'))
        .and_then(|r| r.strip_suffix(".tmp"))
    else {
        return false;
    };
    let Some((pid, ticket)) = rest.split_once('.') else {
        return false;
    };
    pid.parse::<u64>().is_ok() && ticket.parse::<u64>().is_ok()
}

/// Best-effort `0700` on the state dir (same-user-only, like the socket dir).
#[cfg(unix)]
fn tighten_dir(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn tighten_dir(_dir: &Path) {}

/// Parse a `grants.toml` body into grant rows. Pure, so the schema round-trip is
/// unit-testable without disk.
///
/// # Errors
/// If `text` isn't valid TOML for the grant schema.
pub fn parse_grants(text: &str) -> Result<Vec<Grant>, String> {
    let file: GrantsFile = toml::from_str(text).map_err(|e| format!("grants.toml: {e}"))?;
    Ok(file.grant)
}

/// Serialize grant rows back to a `grants.toml` body. Pure.
///
/// # Errors
/// If the rows can't be serialized (not expected for the closed schema).
pub fn to_toml(grants: &[Grant]) -> Result<String, String> {
    let file = GrantsFile {
        grant: grants.to_vec(),
    };
    toml::to_string_pretty(&file).map_err(|e| format!("encoding grants.toml: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
        [[grant]]\n\
        agent = \"claude\"\n\
        datasource = \"departures\"\n\
        scope = \"*\"\n\
        decision = \"always\"\n\
        \n\
        [[grant]]\n\
        agent = \"scratch\"\n\
        datasource = \"departures\"\n\
        decision = \"deny\"\n";

    #[test]
    fn parses_always_and_deny_and_defaults_scope() {
        let grants = parse_grants(SAMPLE).expect("parses");
        assert_eq!(grants.len(), 2);
        assert_eq!(grants[0].agent, "claude");
        assert_eq!(grants[0].decision, Decision::Always);
        assert_eq!(grants[0].scope, "*");
        assert_eq!(grants[1].decision, Decision::Deny);
        // The second row omits `scope` → defaults to the wildcard.
        assert_eq!(grants[1].scope, SCOPE_ALL);
    }

    #[test]
    fn empty_body_is_an_empty_store() {
        assert!(parse_grants("").expect("parses").is_empty());
    }

    #[test]
    fn malformed_body_is_an_error() {
        let err = parse_grants("[[grant]]\ndecision = ").unwrap_err();
        assert!(err.starts_with("grants.toml:"), "got: {err}");
        // An unknown decision value is also rejected loudly.
        assert!(
            parse_grants("[[grant]]\nagent=\"a\"\ndatasource=\"d\"\ndecision=\"maybe\"\n").is_err()
        );
    }

    #[test]
    fn toml_round_trips_through_parse() {
        let grants = parse_grants(SAMPLE).expect("parses");
        let text = to_toml(&grants).expect("encodes");
        let back = parse_grants(&text).expect("re-parses");
        assert_eq!(grants, back);
    }

    #[test]
    fn decision_for_finds_the_matching_pair() {
        let store = GrantStore::from_grants(parse_grants(SAMPLE).unwrap());
        assert_eq!(
            store.decision_for("claude", "departures"),
            Some(Decision::Always)
        );
        assert_eq!(
            store.decision_for("scratch", "departures"),
            Some(Decision::Deny)
        );
        assert_eq!(store.decision_for("nobody", "departures"), None);
        assert_eq!(store.decision_for("claude", "weather"), None);
    }

    #[test]
    fn has_any_always_gates_auth() {
        let store = GrantStore::from_grants(parse_grants(SAMPLE).unwrap());
        assert!(
            store.has_any_always("claude"),
            "an always grant covers auth"
        );
        assert!(
            !store.has_any_always("scratch"),
            "a deny-only agent has no always grant → auth denied"
        );
        assert!(!store.has_any_always("nobody"));
    }

    #[test]
    fn grant_always_is_idempotent_and_revoke_reports_removal() {
        let mut store = GrantStore::from_grants(Vec::new());
        store.grant_always("claude", "departures");
        store.grant_always("claude", "departures");
        assert_eq!(
            store.grants().len(),
            1,
            "a re-grant updates in place, no dup"
        );
        assert_eq!(
            store.decision_for("claude", "departures"),
            Some(Decision::Always)
        );

        assert!(store.revoke("claude", "departures"), "row removed");
        assert!(store.decision_for("claude", "departures").is_none());
        assert!(
            !store.revoke("claude", "departures"),
            "revoking a missing grant reports false"
        );
    }

    #[test]
    fn grant_deny_persists_and_downgrades_an_always_in_place() {
        let mut store = GrantStore::from_grants(Vec::new());
        // A fresh deny is added…
        store.grant_deny("scratch", "departures");
        assert_eq!(store.grants().len(), 1);
        assert_eq!(
            store.decision_for("scratch", "departures"),
            Some(Decision::Deny)
        );
        assert!(store.has_any_deny("scratch"));
        assert!(!store.has_any_always("scratch"));

        // …and an existing `always` is downgraded in place (no dup).
        store.grant_always("claude", "departures");
        store.grant_deny("claude", "departures");
        assert_eq!(store.grants().len(), 2, "downgrade updates in place");
        assert_eq!(
            store.decision_for("claude", "departures"),
            Some(Decision::Deny)
        );
        assert!(!store.has_any_always("claude"));
    }

    #[test]
    fn grant_always_upgrades_a_deny_in_place() {
        let mut store = GrantStore::from_grants(vec![Grant {
            agent: "scratch".to_owned(),
            datasource: "departures".to_owned(),
            scope: SCOPE_ALL.to_owned(),
            decision: Decision::Deny,
        }]);
        store.grant_always("scratch", "departures");
        assert_eq!(store.grants().len(), 1);
        assert_eq!(
            store.decision_for("scratch", "departures"),
            Some(Decision::Always)
        );
    }

    #[test]
    fn load_missing_file_is_empty_store() {
        let path = std::env::temp_dir().join("hytte-infobroker-test-does-not-exist-42.toml");
        let _ = std::fs::remove_file(&path);
        let store = GrantStore::load(&path).expect("missing file → empty");
        assert!(store.grants().is_empty());
    }

    // ── `write_atomic` (#1065) ───────────────────────────────────────────────

    #[test]
    fn write_atomic_creates_parent_and_writes_body() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("grants.toml");
        write_atomic(&path, "hello\n").expect("writes");
        assert_eq!(std::fs::read_to_string(&path).expect("reads"), "hello\n");
        // No temp litter left behind in the parent it just created.
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .expect("reads dir")
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "left a tmp file: {leftovers:?}");
    }

    #[test]
    fn write_atomic_replaces_existing_content() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("grants.toml");
        write_atomic(&path, "old\n").expect("writes");
        write_atomic(&path, "new\n").expect("overwrites");
        assert_eq!(std::fs::read_to_string(&path).expect("reads"), "new\n");
        // Only the target remains — no orphaned tmp file from either write.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .expect("reads dir")
            .filter_map(std::result::Result::ok)
            .map(|e| e.file_name())
            .collect();
        assert_eq!(entries, vec![std::ffi::OsString::from("grants.toml")]);
    }

    #[test]
    fn write_atomic_uses_distinct_tmp_names_across_calls() {
        // Two overlapping writers must never share one tmp path (see
        // `TMP_TICKET`'s doc) — same pid, so only the ticket can tell them
        // apart. Exercised indirectly: two `write_atomic` calls on the same
        // target must not error out from a tmp-path collision.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("grants.toml");
        for i in 0..5 {
            write_atomic(&path, &format!("body {i}\n"))
                .unwrap_or_else(|e| panic!("write {i}: {e}"));
        }
        assert_eq!(std::fs::read_to_string(&path).expect("reads"), "body 4\n");
    }

    /// `save` (reached only through `grant_always`/`grant_deny`/`revoke`) is
    /// fire-and-forget: it queues the write on the blocking pool and returns
    /// before it lands. A `GrantStore` with a real backing path only comes
    /// from `load`, so this seeds an empty file first and then polls for the
    /// mutation to actually reach disk — proof the detached path really does
    /// persist, not just that it doesn't panic.
    #[tokio::test]
    async fn grant_always_persists_to_disk_off_thread() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("grants.toml");
        std::fs::write(&path, "").expect("seed empty file");

        let mut store = GrantStore::load(&path).expect("loads the empty seed");
        store.grant_always("claude", "departures");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            if text.contains("claude") {
                let grants = parse_grants(&text).expect("valid toml");
                assert_eq!(grants, vec![Grant::always("claude", "departures")]);
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "grant_always never reached disk within 2s (got: {text:?})"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    /// The #1065 property, the #1059 way: an injected **slow** write step
    /// must not delay a concurrent 100 ms timer on the same current-thread
    /// runtime. `#[tokio::test]` defaults to exactly that flavor — the same
    /// one `hytte-plugin/src/runtime.rs` runs `serve` on in production.
    ///
    /// Before this restructure `save` ran `write_atomic` inline under
    /// `state`'s mutable borrow; a slow write (a wedged/slow disk, or just a
    /// giant grant table) would have held the only thread the runtime has,
    /// starving every other task on it — the same class #1059 already fixed
    /// for `GrantStore::load`. `save_with` moving the writer to
    /// `spawn_blocking` is what this test pins: reverting that one call
    /// (mutation table, PR body) makes this red at ~2 s instead of ~100 ms.
    #[tokio::test]
    async fn save_offloads_a_slow_writer_without_delaying_a_concurrent_timer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("grants.toml");
        std::fs::write(&path, "").expect("seed empty file");
        let store = GrantStore::load(&path).expect("loads the empty seed");

        let start = std::time::Instant::now();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            start.elapsed()
        });

        // The injected "writer": two seconds of `std::thread::sleep` standing
        // in for a slow/wedged disk. Real `write_atomic` never sleeps.
        store.save_with(|_path, _text| {
            std::thread::sleep(std::time::Duration::from_secs(2));
            Ok(())
        });

        let elapsed = timer
            .await
            .expect("the concurrent 100ms timer task must not panic");
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "the 100ms timer fired at {elapsed:?} — the injected 2s writer stalled this \
             runtime's other tasks, so `save` is not actually off-thread",
        );
    }

    /// #1074 review M1, with the writer seam: two `save`s issued back to back
    /// must land in **submission order**, so the older snapshot can never be
    /// the one left on disk. The first version of this change spawned a
    /// detached `spawn_blocking` per save, and the blocking pool runs those on
    /// different threads with no ordering — so the *earlier* snapshot's
    /// `rename(2)` could land after the later one's and then stand forever,
    /// because nothing ever re-writes the file. In grant terms: a revoked
    /// grant silently comes back at the next broker restart.
    ///
    /// The slow first writer is what makes it deterministic **under the
    /// mutation**; on the fixed tree it never runs at all, and that is
    /// correct. Both saves are issued with no yield between them on a
    /// current-thread runtime, so both jobs are in the channel before the
    /// drain task is first polled and the coalescing loop drops job 1 unrun
    /// (measured: `slow_writer_ran=0 fast_writer_ran=1`). What this test
    /// therefore pins on the fixed tree is **latest-wins**; what it pins
    /// under the mutation is submission order.
    /// `two_uncoalesced_saves_are_written_in_order` below covers the other
    /// half — two jobs the drain picks up separately, neither coalesced —
    /// since coalescing here would otherwise mask a regression in exactly
    /// that path.
    ///
    /// The settle is a fixed sleep rather than a poll on purpose: polling for
    /// "the revoke is on disk" would see the *fast* second write land first
    /// and pass, missing the slow first write inverting it 300 ms later.
    #[tokio::test]
    async fn overlapping_saves_land_out_of_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("grants.toml");
        std::fs::write(&path, "").expect("seed empty file");
        let mut store = GrantStore::load(&path).expect("loads the empty seed");

        // Click 1 — Allow claude/departures, with a slow (busy-disk) write.
        store.grants.push(Grant::always("claude", "departures"));
        store.save_with(|p, t| {
            std::thread::sleep(std::time::Duration::from_millis(300));
            write_atomic(p, t)
        });
        // Click 2, a moment later — Revoke it again. Fast write.
        store.grants.clear();
        store.save_with(write_atomic);

        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let on_disk = parse_grants(&std::fs::read_to_string(&path).expect("reads")).expect("toml");
        assert!(
            on_disk.is_empty(),
            "grants.toml permanently holds the PRE-revoke snapshot {on_disk:?} — the \
             revoked grant comes back at the next broker restart",
        );
    }

    /// The mechanism `spawn_writer`'s doc leads with, which coalescing hides
    /// from the test above (#1074 re-review): **two jobs the drain task picks
    /// up separately**, neither collapsed into the other, must be written in
    /// submission order — and both must actually run.
    ///
    /// What separates the two saves is job 1's writer **signalling that it has
    /// started** — not a fixed sleep, which under load could let job 2 be
    /// queued before the drain task is first polled and put us back in the
    /// coalescing case this test exists to escape. Once job 1 is out of the
    /// channel and inside its `spawn_blocking`, the coalescing `try_recv` loop
    /// cannot swallow it, so both writers run and the order is the lane's
    /// rather than the pool's. Injected writers record their order, so a swap
    /// is visible as more than just the final content.
    #[tokio::test]
    async fn two_uncoalesced_saves_are_written_in_order() {
        use std::sync::{Arc, Mutex};

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("grants.toml");
        std::fs::write(&path, "").expect("seed empty file");
        let mut store = GrantStore::load(&path).expect("loads the empty seed");

        let order: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));

        // Job 1 — a slow write, so the drain task is still inside it when
        // job 2 is queued and the coalescing loop cannot swallow job 1.
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        store.grants.push(Grant::always("claude", "departures"));
        let first = Arc::clone(&order);
        store.save_with(move |p, t| {
            started_tx.send(()).expect("the test outlives this writer");
            std::thread::sleep(std::time::Duration::from_millis(200));
            first.lock().expect("order lock").push("first");
            write_atomic(p, t)
        });
        // Wait for job 1 to be *out of the channel and running*, so the
        // coalescing loop cannot reach it — no fixed sleep to race under load.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while started_rx.try_recv().is_err() {
            assert!(
                std::time::Instant::now() < deadline,
                "the first write never started — the drain task was never polled",
            );
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }

        // Job 2 — the newer snapshot, queued while job 1 is mid-write.
        store.grants.clear();
        let second = Arc::clone(&order);
        store.save_with(move |p, t| {
            second.lock().expect("order lock").push("second");
            write_atomic(p, t)
        });

        // Both writers must run; poll rather than sleep a fixed window, then
        // settle briefly so a *third* (impossible) run would still be seen.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while order.lock().expect("order lock").len() < 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "only {:?} ran within 10s — a queued job was coalesced away after the \
                 drain task had already taken the one before it",
                *order.lock().expect("order lock"),
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(
            *order.lock().expect("order lock"),
            vec!["first", "second"],
            "both queued writes must run, in submission order — a coalesced or \
             reordered lane shows up here before it shows up on disk",
        );
        let on_disk = parse_grants(&std::fs::read_to_string(&path).expect("reads")).expect("toml");
        assert!(
            on_disk.is_empty(),
            "the newer (revoke) snapshot must be the one left on disk, got {on_disk:?}",
        );
    }

    /// The same inversion through the **public API only** — exactly what
    /// `apply_cmd` does for two `Cmd`s already buffered on the command lane:
    /// no seam, no injected delay, the real `write_atomic`. The reviewer
    /// measured 16/100 this way on a 1-row table; the detached-save mutation
    /// reproduces at 4–8/100 here.
    ///
    /// **One row, and one trial at a time, on purpose.** Both are load-bearing
    /// for detection, and both were measured: batching the trials so the
    /// blocking pool is busy takes the rate to *zero*, because the two writes
    /// then each wait on a cold thread spawn in submission order — the
    /// inversion needs a *warm idle* pool worker to steal the second write
    /// while the first is still starting, which is what a serial trial leaves
    /// behind. And the rate falls with table size (the reviewer's own table:
    /// 16/100 at 1 row, 2/100 at 10, 0 at 200) because the `to_toml` that runs
    /// synchronously under the borrow in the second `save` gives the first
    /// write a head start. One row is both the highest-signal fixture and the
    /// realistic size of a personal `grants.toml`.
    ///
    /// Each file is seeded with a sentinel row that is in neither snapshot, so
    /// "no victim on disk" can never be satisfied by a write that did not
    /// happen at all; the settle between "a snapshot landed" and the verdict
    /// is what makes the reading quiescent rather than a race against the
    /// second write.
    #[tokio::test]
    async fn public_api_saves_never_land_out_of_order() {
        const TRIALS: usize = 100;
        const ROWS: usize = 1;
        const SEED: &str = "[[grant]]\nagent = \"seed-sentinel\"\n\
                            datasource = \"departures\"\nscope = \"*\"\ndecision = \"deny\"\n";
        const SETTLE: std::time::Duration = std::time::Duration::from_millis(50);

        let mut inversions = 0usize;
        for _ in 0..TRIALS {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("grants.toml");
            std::fs::write(&path, SEED).expect("seed");
            let mut store = GrantStore::load(&path).expect("loads");
            // Pre-populate without saving, so only the two calls below write.
            store.grants = (0..ROWS)
                .map(|i| Grant::always(format!("agent-{i:05}"), "departures"))
                .collect();

            store.grant_always("victim", "departures");
            assert!(store.revoke("victim", "departures"));

            // Wait for *a* snapshot to land (the seed row is in neither, so
            // this cannot be satisfied by a store that never wrote at all),
            // then settle, so a *late* inverting write is seen rather than
            // raced past.
            await_landed(&path, "seed-sentinel", false).await.expect(
                "neither save reached disk within 2s — this trial cannot tell \
                         ordering from a store that never wrote at all",
            );
            tokio::time::sleep(SETTLE).await;
            // An inversion is permanent — nothing re-writes the file — so the
            // deadline here separates "the older snapshot won" from "the
            // newer one just hasn't been written yet on a loaded machine".
            if await_landed(&path, "victim", false).await.is_err() {
                inversions += 1;
            }
        }
        assert_eq!(
            inversions, 0,
            "{inversions}/{TRIALS} revokes left the revoked grant on disk: an Allow and a \
             Revoke applied back to back (what `apply_cmd` does for two `Cmd`s already \
             buffered on the lane) completed out of order, and the pre-revoke snapshot \
             stands — the grant comes back at the next broker restart",
        );
    }

    /// Poll `path` until `agent`'s presence in the on-disk snapshot is
    /// `want`, or 2 s pass. `Err` means it never got there.
    async fn await_landed(path: &Path, agent: &str, want: bool) -> Result<(), ()> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if on_disk(path).iter().any(|g| g.agent == agent) == want {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    }

    /// Read and parse `path`, treating an unreadable/unparseable file as
    /// "nothing readable yet" — a helper for the ordering trials above, which
    /// poll files a writer task may be replacing under them.
    fn on_disk(path: &Path) -> Vec<Grant> {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| parse_grants(&text).ok())
            .unwrap_or_default()
    }

    /// #1074 review M4: `grant_always`/`grant_deny`/`revoke` are `pub` on a
    /// `pub` type, and the runtime-less `hytte-infobroker` CLI links this same
    /// library — so a mutation with no reactor in context must persist
    /// inline rather than panic with "there is no reactor running".
    ///
    /// A plain `#[test]`, deliberately: `#[tokio::test]` would supply the very
    /// runtime this is asserting we don't need. The write is inline, so it has
    /// already landed by the time `grant_always` returns — no polling.
    #[test]
    fn save_without_a_runtime_writes_inline() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("grants.toml");
        std::fs::write(&path, "").expect("seed empty file");

        let mut store = GrantStore::load(&path).expect("loads the empty seed");
        store.grant_always("claude", "departures");
        assert_eq!(
            parse_grants(&std::fs::read_to_string(&path).expect("reads")).expect("valid toml"),
            vec![Grant::always("claude", "departures")],
            "with no tokio runtime the write must happen inline, not be dropped",
        );

        assert!(store.revoke("claude", "departures"));
        assert!(
            parse_grants(&std::fs::read_to_string(&path).expect("reads"))
                .expect("valid toml")
                .is_empty(),
            "the inline path must serve every mutation, not just the first",
        );
    }

    /// #1074 re-review M6: a `rename(2)` carries the **temp file's** mode onto
    /// the target, so a temp born at the process umask silently re-opens
    /// `grants.toml` on every save. `main`'s `std::fs::write` was a
    /// `create+truncate` open and left an existing file's mode alone;
    /// `hytte_config::file::write_atomic` preserves it deliberately. Measured
    /// before the fix: 0600 → **0644** after one save.
    ///
    /// Runs through the real `write_atomic` with no runtime (the M4 inline
    /// path), so this is the shipping code path and not an injected writer.
    #[cfg(unix)]
    #[test]
    fn save_preserves_the_files_mode_and_defaults_to_0600() {
        use std::os::unix::fs::PermissionsExt;

        let mode_of = |p: &Path| std::fs::metadata(p).expect("stat").permissions().mode() & 0o7777;

        // A file the user has already tightened must stay tightened.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("grants.toml");
        std::fs::write(&path, "").expect("seed empty file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod 600");
        let mut store = GrantStore::load(&path).expect("loads the empty seed");
        store.grant_always("claude", "departures");
        assert_eq!(
            mode_of(&path),
            0o600,
            "a save re-opened the grant store's mode — `chmod 600 grants.toml` must \
             survive a grant change, as it did before the tmp+rename",
        );

        // An unusual-but-deliberate mode is preserved too, not normalised.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).expect("chmod 640");
        store.grant_deny("scratch", "departures");
        assert_eq!(
            mode_of(&path),
            0o640,
            "the target's own mode is what's kept"
        );

        // First run — no file yet, so there is no mode to preserve. Grants
        // are secrets-adjacent: default tight, not to the umask.
        let fresh_dir = tempfile::tempdir().expect("tempdir");
        let fresh = fresh_dir.path().join("grants.toml");
        let mut store = GrantStore::load(&fresh).expect("missing file → empty store");
        store.grant_always("claude", "departures");
        assert_eq!(
            mode_of(&fresh),
            0o600,
            "a grants.toml created from scratch must not be born at the umask",
        );
    }

    /// #1074 re-review M10: the third arm of `save_with` — the writer task is
    /// gone (its runtime is shutting down), `send` fails, and the closure has
    /// to come back out of `SendError` and run inline rather than be dropped
    /// with the grant change in it.
    ///
    /// Built the way the review built it: the lane is created under runtime A,
    /// A is dropped (cancelling the drain task and closing the receiver), and
    /// the next save happens under runtime B — where `try_current()` succeeds,
    /// so the no-runtime arm is *not* what catches this.
    #[test]
    fn a_save_whose_writer_task_is_gone_still_lands_inline() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("grants.toml");
        std::fs::write(&path, "").expect("seed empty file");
        let mut store = GrantStore::load(&path).expect("loads the empty seed");

        let rt_a = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime A");
        rt_a.block_on(async {
            store.grant_always("claude", "departures");
            // Let the drain task run once, so the lane is live rather than
            // merely constructed.
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
        });
        drop(rt_a); // cancels the drain task; the receiver goes with it
        assert!(
            store.writer.get().is_some(),
            "the lane must already exist, or this test is exercising the cold path",
        );

        let rt_b = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime B");
        rt_b.block_on(async {
            store.grant_always("scratch", "weather");
        });

        // No polling: the fallback is a synchronous write, so it has landed
        // by the time `grant_always` returned.
        let on_disk = parse_grants(&std::fs::read_to_string(&path).expect("reads")).expect("toml");
        assert_eq!(
            on_disk,
            vec![
                Grant::always("claude", "departures"),
                Grant::always("scratch", "weather"),
            ],
            "a save queued on a dead writer lane must fall back to the inline write, \
             not be silently dropped along with the closure",
        );
    }

    /// #1074 review M5: a `systemctl stop` inside a write window leaves a
    /// temp sibling that nothing ever cleans up (`run() -> !` never returns
    /// and there is no signal handler), so opening the store sweeps the
    /// crash litter — but only litter: a temp file young enough to belong to
    /// a *live* write (the CLI can `load` while the broker writes) is spared,
    /// as is anything that isn't this file's temp.
    #[test]
    fn load_sweeps_stale_tmp_siblings_and_spares_live_ones() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("grants.toml");
        std::fs::write(&path, "").expect("seed");

        let stale = dir.path().join(".grants.toml.4242.0.tmp");
        let fresh = dir.path().join(".grants.toml.4243.1.tmp");
        let other = dir.path().join(".places.toml.4242.0.tmp");
        let unrelated = dir.path().join("notes.txt");
        // A hand-made backup that merely *looks* like our temp naming: right
        // prefix, right suffix, but the middle is not `<pid>.<ticket>`
        // (#1074 re-review M9 — the reviewer's own file, deleted by the
        // prefix+suffix match this replaces).
        let lookalike = dir
            .path()
            .join(".grants.toml.backup-before-i-edited-it.tmp");
        // …and one that has the right number of fields but not numbers.
        let lookalike2 = dir.path().join(".grants.toml.mine.0.tmp");
        for p in [&stale, &fresh, &other, &unrelated, &lookalike, &lookalike2] {
            std::fs::write(p, "body\n").expect("writes");
        }
        // Age everything that is meant to look like crash litter — including
        // the lookalikes, so age is never what spares them.
        for p in [&stale, &other, &lookalike, &lookalike2] {
            let file = std::fs::File::options().write(true).open(p).expect("opens");
            let old = std::time::SystemTime::now() - (STALE_TMP_AGE + Duration::from_mins(1));
            file.set_times(std::fs::FileTimes::new().set_modified(old))
                .expect("backdates");
        }

        GrantStore::load(&path).expect("loads");

        assert!(!stale.exists(), "a stale temp sibling must be swept");
        assert!(
            fresh.exists(),
            "a temp file young enough to be a live write must be spared — the CLI loads \
             the store while the broker may be mid-write",
        );
        assert!(
            other.exists(),
            "only THIS file's temp siblings are ours to delete",
        );
        assert!(unrelated.exists(), "a non-temp sibling is never touched");
        assert!(
            lookalike.exists(),
            "only names this module could have MINTED are ours to delete — \
             `.grants.toml.<pid>.<ticket>.tmp`, both numbers. A hand-made backup that \
             happens to share the prefix and the suffix is somebody else's file, and \
             deleting it while logging \"left by a crash mid-write\" is a lie as well as \
             a loss",
        );
        assert!(
            lookalike2.exists(),
            "the two middle fields must PARSE as integers, not merely be present",
        );
    }

    /// The #1065 atomicity property: a concurrent reader of `grants.toml`
    /// must at every instant see either the whole *old* snapshot or the
    /// whole *new* one — never a torn mix, and never a parse failure. This is
    /// PR #1064's review finding F2: since #1059 made the session-start grant
    /// *load* genuinely concurrent with a `save`'s write for the first time,
    /// a non-atomic write could hand a reader a half-written file.
    ///
    /// One writer thread alternates between two very differently-sized,
    /// content-distinguishable snapshots (`small`/`large`) many times over,
    /// while this thread polls the file and — whenever it can read *and*
    /// parse it — asserts the result is exactly one of those two snapshots.
    /// Checking against the *known values*, not just "did it parse", is the
    /// point: an interrupted `std::fs::write`'s `O_TRUNC` can leave a reader
    /// looking at a perfectly valid-TOML *empty* file, which `parse_grants`
    /// accepts (`empty_body_is_an_empty_store`) — a plain "parse succeeded"
    /// check would miss exactly that failure mode.
    #[test]
    fn write_atomic_never_exposes_a_torn_file_to_a_concurrent_reader() {
        const ITERATIONS: usize = 20_000;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("grants.toml");

        let small_grants = vec![Grant::always("a", "departures")];
        let small = to_toml(&small_grants).expect("encodes");
        let large_grants: Vec<Grant> = (0..4000)
            .map(|i| Grant {
                agent: format!("agent-{i:05}"),
                datasource: "departures".to_owned(),
                scope: SCOPE_ALL.to_owned(),
                decision: Decision::Deny,
            })
            .collect();
        let large = to_toml(&large_grants).expect("encodes");

        // Seed so the reader's very first read is a real (if racy) target,
        // not "file doesn't exist yet".
        write_atomic(&path, &small).expect("seed write");

        let writer_path = path.clone();
        let (small_w, large_w) = (small.clone(), large.clone());
        let writer = std::thread::spawn(move || {
            for i in 0..ITERATIONS {
                let body = if i % 2 == 0 { &small_w } else { &large_w };
                write_atomic(&writer_path, body).expect("write_atomic");
            }
        });

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut reads = 0usize;
        while !writer.is_finished() && std::time::Instant::now() < deadline {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue; // a transient read error is not tearing; keep polling
            };
            // Only reachable if `write_atomic` regresses to a non-atomic
            // truncate-then-write; a real rename(2) never exposes this.
            assert!(
                !text.is_empty(),
                "read a fully empty grants.toml mid-write — a torn (truncated) write"
            );
            reads += 1;
            match parse_grants(&text) {
                Ok(grants) => assert!(
                    grants == small_grants || grants == large_grants,
                    "read {} grants (first: {:?}) — neither the old nor the new snapshot, \
                     i.e. a torn write",
                    grants.len(),
                    grants.first().map(|g| &g.agent),
                ),
                Err(e) => panic!("read invalid TOML mid-write (a torn write): {e}\n---\n{text}"),
            }
        }
        writer.join().expect("writer thread panicked");
        assert!(
            reads > 50,
            "the reader only raced the writer {reads} times — widen the fixture or \
             iteration count so this test actually contends",
        );
    }
}
