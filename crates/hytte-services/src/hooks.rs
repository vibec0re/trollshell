//! User-authored hook scripts fired on trollshell events.
//!
//! Resolves `$HOME/.config/trollshell/hooks/<event>` and spawns it with
//! caller-supplied env vars plus `TROLLSHELL_EVENT`. Fire-and-forget from
//! the caller's POV: all outcomes go to `tracing`. See
//! `docs/superpowers/specs/2026-05-05-settings-hooks-design.md`.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

#[cfg(not(test))]
const HOOK_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(test)]
const HOOK_TIMEOUT: Duration = Duration::from_millis(500);

/// Cap on captured stdout/stderr, per pipe (#1171). Without this, a chatty
/// (or runaway) hook script had its output drained fully into memory before
/// `HOOK_TIMEOUT` had a chance to matter — a script printing tens of
/// megabytes ballooned this process's RSS well within the timeout window,
/// not just at its edge. Sized generously relative to `effects.rs`'s
/// `RUN_COMMAND_MAX_OUTPUT` (4 KiB): that cap bounds a wire reply to a
/// plugin, this one bounds a diagnostic `tracing` field for the user's own
/// script.
const HOOK_OUTPUT_BUDGET: usize = 64 * 1024;

/// Run the user's hook script for `event`, if one exists.
///
/// Returns immediately. The actual spawn + wait happens on the
/// `hytte_reactive` tokio runtime (or the current runtime, if any).
/// Outcomes are logged via `tracing`; errors never propagate.
pub fn run(event: &str, env: &[(&str, &str)]) {
    let event = event.to_string();
    let env: Vec<(String, String)> = env
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    spawn_task(async move { run_inner(&event, &env).await });
}

fn spawn_task<F>(fut: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(fut);
        return;
    }
    hytte_reactive::runtime::handle().spawn(fut);
}

async fn run_inner(event: &str, env: &[(String, String)]) {
    let Some(path) = resolve_path(event) else {
        return;
    };
    let meta = match tokio::fs::metadata(&path).await {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!(event, path = %path.display(), "hooks: no script configured");
            return;
        }
        Err(e) => {
            tracing::warn!(event, path = %path.display(), error = %e, "hooks: stat failed");
            return;
        }
    };
    if !meta.is_file() {
        tracing::warn!(event, path = %path.display(), "hooks: not a regular file");
        return;
    }
    if meta.permissions().mode() & 0o111 == 0 {
        tracing::warn!(event, path = %path.display(), "hooks: script not executable");
        return;
    }

    let mut cmd = tokio::process::Command::new(&path);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .env("TROLLSHELL_EVENT", event);
    for (k, v) in env {
        cmd.env(k, v);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(event, path = %path.display(), error = %e, "hooks: spawn failed");
            return;
        }
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let read_outputs = async {
        tokio::join!(
            drain(stdout, HOOK_OUTPUT_BUDGET),
            drain(stderr, HOOK_OUTPUT_BUDGET)
        )
    };
    let wait = async { tokio::join!(read_outputs, child.wait()) };

    match tokio::time::timeout(HOOK_TIMEOUT, wait).await {
        Ok((((sout, sout_truncated), (serr, serr_truncated)), Ok(status))) if status.success() => {
            tracing::info!(event, "hooks: ran");
            if !sout.is_empty() {
                tracing::info!(
                    event,
                    stdout = %String::from_utf8_lossy(&sout),
                    truncated = sout_truncated,
                    "hooks: stdout",
                );
            }
            if !serr.is_empty() {
                tracing::info!(
                    event,
                    stderr = %String::from_utf8_lossy(&serr),
                    truncated = serr_truncated,
                    "hooks: stderr",
                );
            }
        }
        Ok((((sout, sout_truncated), (serr, serr_truncated)), Ok(status))) => {
            tracing::warn!(
                event,
                status = ?status,
                stdout = %String::from_utf8_lossy(&sout),
                stdout_truncated = sout_truncated,
                stderr = %String::from_utf8_lossy(&serr),
                stderr_truncated = serr_truncated,
                "hooks: script failed",
            );
        }
        Ok((_outputs, Err(e))) => {
            tracing::warn!(event, error = %e, "hooks: wait failed");
        }
        Err(_) => {
            tracing::warn!(event, "hooks: script timed out, killing");
        }
    }
}

fn resolve_path(event: &str) -> Option<PathBuf> {
    let Some(home) = std::env::var_os("HOME") else {
        tracing::warn!(event, "hooks: $HOME not set");
        return None;
    };
    Some(
        PathBuf::from(home)
            .join(".config/trollshell/hooks")
            .join(event),
    )
}

/// Drain `stream` into a buffer capped at `budget` bytes, returning
/// `(captured, truncated)`. Replaces the old `read_to_end`-with-no-cap
/// shape (#1171): `.take(budget + 1)` reads at most one byte past the
/// budget, which is enough to tell "exactly `budget` bytes, nothing more"
/// apart from "there was more" without a separate EOF probe.
///
/// A truncated read then keeps draining (and discarding) the rest of the
/// stream rather than stopping outright — stopping would leave the pipe's
/// kernel buffer full and the writer blocked on its next `write()`, which
/// would turn "printed more than the budget" into "hangs until
/// `HOOK_TIMEOUT` kills it", discarding the very output the budget exists
/// to preserve (see the `chatty_stdout_is_capped_at_budget` test).
///
/// That discard loop also stops on a read **error**, not only on EOF
/// (#1192 review, NIT-3): `while matches!(…, Ok(n) if n > 0)` exits on
/// `Err(_)` too, leaving the pipe unread. That is deliberate — retrying a
/// broken pipe read would spin — and it is bounded from the other side by
/// `kill_on_drop(true)` plus `HOOK_TIMEOUT`, so the writer cannot be left
/// blocked indefinitely either way.
async fn drain<R>(stream: Option<R>, budget: usize) -> (Vec<u8>, bool)
where
    R: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;
    let Some(mut s) = stream else {
        return (Vec::new(), false);
    };
    let mut buf = Vec::new();
    let extended_budget = u64::try_from(budget).unwrap_or(u64::MAX).saturating_add(1);
    let _ = (&mut s).take(extended_budget).read_to_end(&mut buf).await;
    let truncated = buf.len() > budget;
    if truncated {
        buf.truncate(budget);
        // Heap-allocated, not a stack array: a stack buffer held across the
        // `.await` below lives inside this fn's generated future, and two of
        // those (stdout + stderr, `run_inner` joins them) pushed the whole
        // call tree well past clippy::large_futures. A `Vec`'s backing bytes
        // live on the heap regardless — the future only stores the 24-byte
        // pointer/len/cap triple across the suspend point.
        let mut sink = vec![0u8; 8192];
        while matches!(s.read(&mut sink).await, Ok(n) if n > 0) {}
    }
    (buf, truncated)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tracing::{Event, Subscriber};
    use tracing_subscriber::Registry;
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    pub(super) struct TestHome {
        pub root: PathBuf,
    }

    impl TestHome {
        /// Run `f` with `$HOME` temporarily set to a fresh tempdir, awaiting
        /// the future it returns. `temp_env::async_with_vars` serializes env
        /// mutation across tests and restores the previous value on return
        /// or panic.
        pub async fn with<F, Fut, R>(f: F) -> R
        where
            F: FnOnce(TestHome) -> Fut,
            Fut: std::future::Future<Output = R>,
        {
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!("hytte-hooks-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            let cleanup = root.clone();
            let home = TestHome { root: root.clone() };
            let result =
                temp_env::async_with_vars([("HOME", Some(root.into_os_string()))], f(home)).await;
            let _ = std::fs::remove_dir_all(&cleanup);
            result
        }

        pub fn hooks_dir(&self) -> PathBuf {
            let dir = self.root.join(".config/trollshell/hooks");
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        pub fn write_script(&self, event: &str, body: &str, mode: u32) -> PathBuf {
            use std::os::unix::fs::PermissionsExt;
            let path = self.hooks_dir().join(event);
            std::fs::write(&path, body).unwrap();
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(mode);
            std::fs::set_permissions(&path, perms).unwrap();
            path
        }
    }

    #[derive(Clone, Debug)]
    pub(super) struct CapturedEvent {
        pub level: tracing::Level,
        pub message: String,
        pub fields: std::collections::HashMap<String, String>,
    }

    #[derive(Default, Clone)]
    pub(super) struct Captured {
        pub events: std::sync::Arc<Mutex<Vec<CapturedEvent>>>,
    }

    impl<S: Subscriber> Layer<S> for Captured {
        fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
            let mut visitor = FieldVisitor::default();
            event.record(&mut visitor);
            self.events.lock().unwrap().push(CapturedEvent {
                level: *event.metadata().level(),
                message: visitor.message,
                fields: visitor.fields,
            });
        }
    }

    #[derive(Default)]
    struct FieldVisitor {
        message: String,
        fields: std::collections::HashMap<String, String>,
    }

    impl tracing::field::Visit for FieldVisitor {
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            if field.name() == "message" {
                self.message = value.to_string();
            } else {
                self.fields
                    .insert(field.name().to_string(), value.to_string());
            }
        }
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            let s = format!("{value:?}");
            if field.name() == "message" {
                self.message = s;
            } else {
                self.fields.insert(field.name().to_string(), s);
            }
        }
    }

    const POLL_STEP: std::time::Duration = std::time::Duration::from_millis(10);
    const POLL_BOUND: std::time::Duration = std::time::Duration::from_secs(2);

    /// Poll every [`POLL_STEP`], up to [`POLL_BOUND`] total, until `ready`
    /// returns `true`. Panics with `describe()`'s message if the bound
    /// elapses first.
    ///
    /// Replaces the old fixed-sleep-then-assert shape (#1028): `run_inner`
    /// (`hooks.rs:46`) awaits `tokio::fs::metadata`, which tokio dispatches
    /// to its blocking pool, before its first `tracing` call — so a flat
    /// sleep is not a bound on when that call lands under a loaded runner.
    async fn poll_until(mut ready: impl FnMut() -> bool, describe: impl FnOnce() -> String) {
        let deadline = std::time::Instant::now() + POLL_BOUND;
        loop {
            if ready() {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "timed out after {POLL_BOUND:?}: {}",
                describe()
            );
            tokio::time::sleep(POLL_STEP).await;
        }
    }

    /// [`poll_until`] specialised for the `Captured` event log: wait for an
    /// event matching `pred` (described by `what`, for the panic message),
    /// then return the full captured list at that point — for asserting on
    /// other fields of the same event, or for a negative assertion (e.g.
    /// "no WARN events") that is only meaningful once the positive one has
    /// actually landed rather than being vacuously true on a timeout that
    /// never happened.
    async fn wait_for(
        cap: &Captured,
        what: &str,
        mut pred: impl FnMut(&CapturedEvent) -> bool,
    ) -> Vec<CapturedEvent> {
        poll_until(
            || cap.events.lock().unwrap().iter().any(&mut pred),
            || {
                let events = cap.events.lock().unwrap().clone();
                format!("waiting for {what}; events seen: {events:#?}")
            },
        )
        .await;
        cap.events.lock().unwrap().clone()
    }

    /// No callsite warm-up here (see #1028 fix-round review, PR #1032): in
    /// `tracing-core` 0.1.36, `Dispatch::new` (`src/dispatcher.rs:479`)
    /// calls `callsite::register_dispatch`, which ends in
    /// `CALLSITES.rebuild_interest(dispatchers)` (`src/callsite.rs:484-487`)
    /// — a rebuild over *every already-registered* callsite against every
    /// live dispatcher. `capture()` below constructs a fresh `Dispatch::new`
    /// on every call, which un-poisons any callsite a prior test cached
    /// `Interest::never()` for. There is no subscriber-less first fire to
    /// guard against in this harness: `spawn_task` (`hooks.rs:35`) prefers
    /// `Handle::try_current()`, which inside `#[tokio::test(flavor =
    /// "current_thread")]` is the test's own runtime, so `run_inner` always
    /// runs on the thread whose thread-local default is already the
    /// capture dispatch. A prior revision of this file warmed up the
    /// callsites anyway; measured to be a no-op (250-run full-binary
    /// campaign, 0 failures) and, worse, itself the exact "subscriber-less
    /// first fire" its own doc comment warned about — removed.
    pub(super) fn capture() -> (Captured, tracing::dispatcher::DefaultGuard) {
        let cap = Captured::default();
        let dispatch = tracing::Dispatch::new(Registry::default().with(cap.clone()));
        let guard = tracing::dispatcher::set_default(&dispatch);
        (cap, guard)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn success_logs_info_and_captures_stdout() {
        TestHome::with(|home| async move {
            home.write_script("theme-changed", "#!/bin/sh\necho hi\nexit 0\n", 0o755);
            let (cap, _guard) = capture();

            super::run("theme-changed", &[]);

            wait_for(&cap, "an INFO 'ran' event", |e| {
                e.level == tracing::Level::INFO
                    && e.message.contains("ran")
                    && e.fields.get("event").is_some_and(|s| s == "theme-changed")
            })
            .await;

            wait_for(&cap, "an INFO event with stdout=hi", |e| {
                e.level == tracing::Level::INFO
                    && e.fields.get("stdout").is_some_and(|s| s.contains("hi"))
                    && e.fields.get("event").is_some_and(|s| s == "theme-changed")
            })
            .await;
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn nonzero_exit_logs_warn_with_outputs() {
        TestHome::with(|home| async move {
            home.write_script(
                "theme-changed",
                "#!/bin/sh\necho boom 1>&2\nexit 7\n",
                0o755,
            );
            let (cap, _guard) = capture();

            super::run("theme-changed", &[]);

            let events = wait_for(&cap, "a WARN 'script failed' event", |e| {
                e.level == tracing::Level::WARN
                    && e.message.contains("failed")
                    && e.fields.get("event").is_some_and(|s| s == "theme-changed")
            })
            .await;

            let warn = events
                .iter()
                .find(|e| {
                    e.level == tracing::Level::WARN
                        && e.message.contains("failed")
                        && e.fields.get("event").is_some_and(|s| s == "theme-changed")
                })
                .expect("wait_for guarantees a matching WARN event is present");
            assert!(
                warn.fields
                    .get("stderr")
                    .is_some_and(|s| s.contains("boom")),
                "expected stderr=boom, got: {:#?}",
                warn.fields,
            );
            assert!(
                warn.fields.get("status").is_some_and(|s| s.contains('7')),
                "expected status to mention 7, got: {:#?}",
                warn.fields,
            );
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn non_executable_warns_and_does_not_run() {
        TestHome::with(|home| async move {
            let sentinel = home.root.join("sentinel");
            let body = format!("#!/bin/sh\ntouch {}\n", sentinel.display());
            home.write_script("theme-changed", &body, 0o644); // no exec bit
            let (cap, _guard) = capture();

            super::run("theme-changed", &[]);

            let events = wait_for(&cap, "a WARN 'not executable' event", |e| {
                e.level == tracing::Level::WARN
                    && e.message.contains("not executable")
                    && e.fields.get("event").is_some_and(|s| s == "theme-changed")
            })
            .await;

            // The WARN above proves the rejection branch ran, but that
            // alone doesn't rule out some other branch *also* spawning the
            // script — assert that at the event level too: no INFO "ran"
            // for this event is present in the capture.
            assert!(
                !events.iter().any(|e| e.level == tracing::Level::INFO
                    && e.message.contains("ran")
                    && e.fields.get("event").is_some_and(|s| s == "theme-changed")),
                "expected no INFO 'ran' event for theme-changed, got: {events:#?}",
            );

            // Not vacuous: the positive wait above already established the
            // script was rejected before this negative check runs.
            assert!(!sentinel.exists(), "script must not have been executed");
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn timeout_kills_child_and_warns() {
        TestHome::with(|home| async move {
            home.write_script("theme-changed", "#!/bin/sh\nsleep 30\n", 0o755);
            let (cap, _guard) = capture();

            let started = std::time::Instant::now();
            super::run("theme-changed", &[]);

            wait_for(&cap, "a WARN 'timed out' event", |e| {
                e.level == tracing::Level::WARN
                    && e.message.contains("timed out")
                    && e.fields.get("event").is_some_and(|s| s == "theme-changed")
            })
            .await;

            let elapsed = started.elapsed();
            assert!(
                elapsed < std::time::Duration::from_secs(5),
                "timeout should fire well before 10s; took {elapsed:?}",
            );
        })
        .await;
    }

    /// #1171: a script printing far more than [`super::HOOK_OUTPUT_BUDGET`]
    /// is captured only up to the budget, marked `truncated`, and — because
    /// `drain` keeps draining (and discarding) past the cap instead of
    /// stopping — the script still finishes normally well inside
    /// `HOOK_TIMEOUT`, rather than hanging on a full pipe until killed.
    ///
    /// **2 MB, not 50 MB** (#1192 review, LOW-3). The budget is 64 KiB, so
    /// 2 MB is 32× over it — everything this test asserts (the cap, the
    /// `truncated` flag, the "didn't hang on a full pipe" timing) holds
    /// identically, while the work done under a **500 ms** `cfg(test)`
    /// `HOOK_TIMEOUT` drops 25-fold. At 50 MB a slow enough run would have
    /// the hook killed instead, the `ran` INFO would never fire, and the
    /// test would hang in `wait_for` rather than failing cleanly — an
    /// unnecessary flake to carry into `nix flake check`, which runs this
    /// beside two nixosTest VMs and the workspace clippy.
    #[tokio::test(flavor = "current_thread")]
    async fn chatty_stdout_is_capped_at_budget() {
        TestHome::with(|home| async move {
            home.write_script(
                "theme-changed",
                "#!/bin/sh\nhead -c 2000000 /dev/zero\nexit 0\n",
                0o755,
            );
            let (cap, _guard) = capture();

            let started = std::time::Instant::now();
            super::run("theme-changed", &[]);

            let events = wait_for(&cap, "an INFO 'ran' event", |e| {
                e.level == tracing::Level::INFO
                    && e.message.contains("ran")
                    && e.fields.get("event").is_some_and(|s| s == "theme-changed")
            })
            .await;

            // Ran to completion, not killed by the timeout — proves the
            // over-budget script wasn't left blocked on a full pipe.
            let elapsed = started.elapsed();
            assert!(
                elapsed < std::time::Duration::from_secs(5),
                "a script draining past its own budget shouldn't hang until \
                 killed; took {elapsed:?}",
            );
            assert!(
                !events
                    .iter()
                    .any(|e| e.level == tracing::Level::WARN && e.message.contains("timed out")),
                "expected no timeout WARN, got: {events:#?}",
            );

            let stdout_event = events
                .iter()
                .find(|e| e.fields.contains_key("stdout"))
                .unwrap_or_else(|| panic!("no event carried stdout: {events:#?}"));
            let captured = stdout_event
                .fields
                .get("stdout")
                .expect("checked above via contains_key");
            assert_eq!(
                captured.len(),
                super::HOOK_OUTPUT_BUDGET,
                "captured stdout should be capped at the budget, not the full \
                 2 MB the script printed",
            );
            assert_eq!(
                stdout_event.fields.get("truncated").map(String::as_str),
                Some("true"),
                "a capped read must be flagged truncated: {stdout_event:#?}",
            );
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn missing_script_logs_debug_only() {
        TestHome::with(|home| async move {
            let _ = home.hooks_dir(); // dir exists, no script written
            let (cap, _guard) = capture();

            super::run("theme-changed", &[]);

            let events = wait_for(&cap, "a DEBUG 'no script' event", |e| {
                e.level == tracing::Level::DEBUG
                    && e.message.contains("no script")
                    && e.fields.get("event").is_some_and(|s| s == "theme-changed")
            })
            .await;

            // Not vacuous: only checked once the positive wait above has
            // actually landed, not after an unconditional fixed delay.
            assert!(
                !events.iter().any(|e| e.level == tracing::Level::WARN),
                "expected no WARN events, got: {events:#?}",
            );
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn env_vars_reach_script() {
        TestHome::with(|home| async move {
            let sentinel = home.root.join("env-out");
            let body = format!(
                "#!/bin/sh\nprintf 'event=%s theme=%s' \"$TROLLSHELL_EVENT\" \"$TROLLSHELL_THEME\" > {}\n",
                sentinel.display(),
            );
            home.write_script("theme-changed", &body, 0o755);
            let (_cap, _guard) = capture();

            super::run("theme-changed", &[("TROLLSHELL_THEME", "dark")]);

            // Poll the content, not the inode: the shell's `>` redirect
            // does `open(O_CREAT|O_TRUNC)` before `printf` writes, so
            // `sentinel.exists()` goes true while the file is still empty
            // — polling existence alone can observe that partial state.
            poll_until(
                || std::fs::read_to_string(&sentinel).is_ok_and(|s| !s.is_empty()),
                || format!("waiting for the script to write {}", sentinel.display()),
            )
            .await;

            let contents = std::fs::read_to_string(&sentinel)
                .expect("script should have written sentinel");
            assert_eq!(contents, "event=theme-changed theme=dark");
        })
        .await;
    }
}
