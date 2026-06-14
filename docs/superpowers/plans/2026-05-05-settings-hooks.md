# Settings Hooks Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add user-authored hook scripts that run on trollshell events. v1 wires only `theme-changed`, fired after `theme::set` completes its toolkit fan-out.

**Architecture:** A new `hytte-services::hooks` module exposes a single fire-and-forget `run(event, env)` function. It resolves `$HOME/.config/trollshell/hooks/<event>`, spawns it detached on the existing `hytte_reactive` tokio runtime with a 10s timeout, captures stdout/stderr, and reports outcomes via `tracing`. `theme::set` calls it once after writing all toolkit state.

**Tech Stack:** Rust 2024, `tokio::process`, `tokio::time`, `tracing`, `hytte_reactive::runtime` (existing tokio handle).

**Spec:** `docs/superpowers/specs/2026-05-05-settings-hooks-design.md`

---

## File Structure

- **Create:** `crates/hytte-services/src/hooks.rs` — the `run` fn, helpers, and `#[cfg(test)] mod tests`.
- **Modify:** `crates/hytte-services/src/lib.rs` — add `pub mod hooks;`.
- **Modify:** `crates/hytte-services/src/theme.rs` — call `hooks::run` at the bottom of `set`.
- **Modify:** `crates/hytte-services/Cargo.toml` — add `tracing-subscriber` to `[dev-dependencies]`.

---

## Test infrastructure notes

The tests mutate `$HOME` (cargo runs tests multi-threaded by default), so all tests acquire a process-wide `Mutex<()>` before touching env. Each test also uses a unique tempdir keyed by `(pid, atomic counter)` so leftover state from a prior run never collides.

Tracing assertions use a per-test `tracing_subscriber::fmt` layer composed with a custom `Layer` that pushes events into a `Vec<CapturedEvent>`. Setting it as the _thread_ default (`with_default`) keeps test isolation simple; combined with the `$HOME` mutex, only one test runs at a time anyway.

The runtime: tests are `#[tokio::test(flavor = "current_thread", start_paused = false)]`. The production code uses `hytte_reactive::runtime::handle().spawn(...)`, but in tests we don't have that runtime initialized — the production fn must therefore detect the absence and fall back to spawning on the _current_ tokio runtime when one is available. Implementation detail: use `tokio::runtime::Handle::try_current()` first; if `Err`, try `hytte_reactive::runtime::handle()`; if neither is available, log a warn and return.

---

## Task 1: Scaffold the module + dev-deps + test harness

**Files:**

- Create: `crates/hytte-services/src/hooks.rs`
- Modify: `crates/hytte-services/src/lib.rs` (add `pub mod hooks;` alphabetically — between `dnd` and `logind`)
- Modify: `crates/hytte-services/Cargo.toml` (`[dev-dependencies]`)

- [ ] **Step 1: Add `tracing-subscriber` to `hytte-services` dev-deps**

Modify `crates/hytte-services/Cargo.toml`. Under `[dev-dependencies]`, append:

```toml
tracing-subscriber = { version = "0.3.23", features = ["fmt", "env-filter"] }
```

(Version pinned to match what's already used in `trollshell/Cargo.toml`.)

- [ ] **Step 2: Create `hooks.rs` with stub + module declaration**

Create `crates/hytte-services/src/hooks.rs`:

```rust
//! User-authored hook scripts fired on trollshell events.
//!
//! Resolves `$HOME/.config/trollshell/hooks/<event>` and spawns it with
//! caller-supplied env vars plus `TROLLSHELL_EVENT`. Fire-and-forget from
//! the caller's POV: all outcomes go to `tracing`. See
//! `docs/superpowers/specs/2026-05-05-settings-hooks-design.md`.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

const HOOK_TIMEOUT: Duration = Duration::from_secs(10);

/// Run the user's hook script for `event`, if one exists.
///
/// Returns immediately. The actual spawn + wait happens on the
/// `hytte_reactive` tokio runtime (or the current runtime, if any).
/// Outcomes are logged via `tracing`; errors never propagate.
pub fn run(event: &str, env: &[(&str, &str)]) {
    let _ = (event, env);
    // TODO(impl in later tasks)
}
```

Add `pub mod hooks;` to `crates/hytte-services/src/lib.rs` between `pub mod dnd;` and `pub mod logind;`.

- [ ] **Step 3: Verify it compiles**

Run: `cargo check -p hytte-services`
Expected: builds clean (the unused `event`/`env` are absorbed by the `let _ = ...`).

- [ ] **Step 4: Add the shared test scaffolding**

Append to `crates/hytte-services/src/hooks.rs` (still in this task — no test bodies yet, just the harness):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;
    use tracing::{Event, Subscriber};
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::Registry;

    /// Serializes tests that mutate `$HOME`.
    static HOME_LOCK: Mutex<()> = Mutex::new(());
    static SEQ: AtomicU64 = AtomicU64::new(0);

    pub(super) struct TestHome {
        _guard: std::sync::MutexGuard<'static, ()>,
        prev_home: Option<std::ffi::OsString>,
        pub root: PathBuf,
    }

    impl TestHome {
        pub fn new() -> Self {
            let guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir()
                .join(format!("hytte-hooks-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            let prev = std::env::var_os("HOME");
            // SAFETY: tests are serialized via HOME_LOCK; no concurrent env access.
            unsafe { std::env::set_var("HOME", &root); }
            Self { _guard: guard, prev_home: prev, root }
        }

        pub fn hooks_dir(&self) -> PathBuf {
            let dir = self.root.join(".config/trollshell/hooks");
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        pub fn write_script(&self, event: &str, body: &str, mode: u32) -> PathBuf {
            let path = self.hooks_dir().join(event);
            std::fs::write(&path, body).unwrap();
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(mode);
            std::fs::set_permissions(&path, perms).unwrap();
            path
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            // SAFETY: serialized via HOME_LOCK.
            unsafe {
                if let Some(prev) = self.prev_home.take() {
                    std::env::set_var("HOME", prev);
                } else {
                    std::env::remove_var("HOME");
                }
            }
            let _ = std::fs::remove_dir_all(&self.root);
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
                self.fields.insert(field.name().to_string(), value.to_string());
            }
        }
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            let s = format!("{:?}", value);
            if field.name() == "message" {
                self.message = s;
            } else {
                self.fields.insert(field.name().to_string(), s);
            }
        }
    }

    pub(super) fn capture() -> (Captured, tracing::dispatcher::DefaultGuard) {
        let cap = Captured::default();
        let dispatch = tracing::Dispatch::new(Registry::default().with(cap.clone()));
        let guard = tracing::dispatcher::set_default(&dispatch);
        (cap, guard)
    }
}
```

- [ ] **Step 5: Verify the harness compiles**

Run: `cargo test -p hytte-services hooks:: --no-run`
Expected: builds clean. No tests yet so nothing runs.

- [ ] **Step 6: Commit**

```bash
git add crates/hytte-services/src/hooks.rs crates/hytte-services/src/lib.rs crates/hytte-services/Cargo.toml
git commit -m "feat(hooks): scaffold module + test harness"
```

---

## Task 2: Missing script → silent debug log

Behavior: when `$HOME/.config/trollshell/hooks/<event>` does not exist, `run` logs at `DEBUG` and emits no `WARN`.

**Files:**

- Modify: `crates/hytte-services/src/hooks.rs`

- [ ] **Step 1: Write the failing test**

Inside the existing `mod tests`, append:

```rust
    #[tokio::test(flavor = "current_thread")]
    async fn missing_script_logs_debug_only() {
        let home = TestHome::new();
        let _ = home.hooks_dir(); // ensure dir exists, but no script written
        let (cap, _guard) = capture();

        super::run("theme-changed", &[]);

        // The spawn happens on a background task; give it a tick to land.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let events = cap.events.lock().unwrap().clone();
        assert!(
            events.iter().any(|e| e.level == tracing::Level::DEBUG
                && e.message.contains("no script")),
            "expected a DEBUG 'no script' event, got: {:#?}",
            events,
        );
        assert!(
            !events.iter().any(|e| e.level == tracing::Level::WARN),
            "expected no WARN events, got: {:#?}",
            events,
        );
    }
```

- [ ] **Step 2: Run the test, verify it fails**

Run: `cargo test -p hytte-services hooks::tests::missing_script_logs_debug_only -- --nocapture`
Expected: FAIL — current `run` is a stub, no events emitted.

- [ ] **Step 3: Implement minimal logic**

Replace the body of `pub fn run` in `hooks.rs` with:

```rust
pub fn run(event: &str, env: &[(&str, &str)]) {
    let event = event.to_string();
    let env: Vec<(String, String)> = env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
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
    // Fall back to the hytte-reactive runtime when not inside one (production callsites).
    hytte_reactive::runtime::handle().spawn(fut);
}

async fn run_inner(event: &str, env: &[(String, String)]) {
    let Some(path) = resolve_path(event) else { return; };
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
    let _ = (meta, env); // remaining branches arrive in later tasks
}

fn resolve_path(event: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").or_else(|| {
        tracing::warn!(event, "hooks: $HOME not set");
        None
    })?;
    Some(PathBuf::from(home).join(".config/trollshell/hooks").join(event))
}
```

(The `let _ = (meta, env);` is a deliberate placeholder so the compiler stays happy until later tasks expand the function — no comment about it in the source code; the placeholder will be deleted as it's filled in.)

- [ ] **Step 4: Run the test, verify it passes**

Run: `cargo test -p hytte-services hooks::tests::missing_script_logs_debug_only -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/hytte-services/src/hooks.rs
git commit -m "feat(hooks): debug-log absent script, warn on missing HOME"
```

---

## Task 3: Successful script → INFO log, captured stdout

Behavior: an executable script that exits 0 is spawned; stdout is captured and logged at `DEBUG`; an `INFO` event marks success.

**Files:**

- Modify: `crates/hytte-services/src/hooks.rs`

- [ ] **Step 1: Write the failing test**

Append to `mod tests`:

```rust
    #[tokio::test(flavor = "current_thread")]
    async fn success_logs_info_and_captures_stdout() {
        let home = TestHome::new();
        home.write_script("theme-changed", "#!/bin/sh\necho hi\nexit 0\n", 0o755);
        let (cap, _guard) = capture();

        super::run("theme-changed", &[]);

        // Wait up to 2s for the script to complete and the task to log.
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            if cap.events.lock().unwrap().iter()
                .any(|e| e.level == tracing::Level::INFO) { break; }
        }

        let events = cap.events.lock().unwrap().clone();
        assert!(
            events.iter().any(|e| e.level == tracing::Level::INFO
                && e.message.contains("ran")),
            "expected an INFO 'ran' event, got: {:#?}",
            events,
        );
        assert!(
            events.iter().any(|e| e.level == tracing::Level::DEBUG
                && e.fields.get("stdout").map_or(false, |s| s.contains("hi"))),
            "expected a DEBUG event with stdout=hi, got: {:#?}",
            events,
        );
    }
```

- [ ] **Step 2: Run the test, verify it fails**

Run: `cargo test -p hytte-services hooks::tests::success_logs_info_and_captures_stdout -- --nocapture`
Expected: FAIL — `run_inner` returns after `metadata()` without spawning.

- [ ] **Step 3: Implement spawn + wait + success log**

Replace the body of `run_inner` in `hooks.rs` with:

```rust
async fn run_inner(event: &str, env: &[(String, String)]) {
    let Some(path) = resolve_path(event) else { return; };
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

    let mut cmd = tokio::process::Command::new(&path);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
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
        use tokio::io::AsyncReadExt;
        let mut sout = Vec::new();
        let mut serr = Vec::new();
        if let Some(mut s) = stdout {
            let _ = s.read_to_end(&mut sout).await;
        }
        if let Some(mut s) = stderr {
            let _ = s.read_to_end(&mut serr).await;
        }
        (sout, serr)
    };

    let wait = async {
        let (out, status) = tokio::join!(read_outputs, child.wait());
        (out, status)
    };

    match tokio::time::timeout(HOOK_TIMEOUT, wait).await {
        Ok(((sout, serr), Ok(status))) if status.success() => {
            tracing::info!(event, "hooks: ran");
            if !sout.is_empty() {
                tracing::debug!(event, stdout = %String::from_utf8_lossy(&sout), "hooks: stdout");
            }
            if !serr.is_empty() {
                tracing::debug!(event, stderr = %String::from_utf8_lossy(&serr), "hooks: stderr");
            }
        }
        Ok(_) | Err(_) => {
            // remaining branches arrive in later tasks
        }
    }
}
```

- [ ] **Step 4: Run the test, verify it passes**

Run: `cargo test -p hytte-services hooks::tests::success_logs_info_and_captures_stdout -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Run prior test to verify no regression**

Run: `cargo test -p hytte-services hooks:: -- --nocapture`
Expected: 2 passed.

- [ ] **Step 6: Commit**

```bash
git add crates/hytte-services/src/hooks.rs
git commit -m "feat(hooks): spawn script, log success with captured output"
```

---

## Task 4: Non-zero exit → WARN with status + stdout + stderr

**Files:**

- Modify: `crates/hytte-services/src/hooks.rs`

- [ ] **Step 1: Write the failing test**

Append to `mod tests`:

```rust
    #[tokio::test(flavor = "current_thread")]
    async fn nonzero_exit_logs_warn_with_outputs() {
        let home = TestHome::new();
        home.write_script(
            "theme-changed",
            "#!/bin/sh\necho boom 1>&2\nexit 7\n",
            0o755,
        );
        let (cap, _guard) = capture();

        super::run("theme-changed", &[]);

        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            if cap.events.lock().unwrap().iter()
                .any(|e| e.level == tracing::Level::WARN) { break; }
        }

        let events = cap.events.lock().unwrap().clone();
        let warn = events.iter().find(|e| e.level == tracing::Level::WARN)
            .unwrap_or_else(|| panic!("expected a WARN event, got: {:#?}", events));
        assert!(warn.message.contains("failed"), "warn msg: {:?}", warn.message);
        assert!(warn.fields.get("stderr").map_or(false, |s| s.contains("boom")),
            "expected stderr=boom, got: {:#?}", warn.fields);
        assert!(warn.fields.get("status").map_or(false, |s| s.contains("7")),
            "expected status to mention 7, got: {:#?}", warn.fields);
    }
```

- [ ] **Step 2: Run the test, verify it fails**

Run: `cargo test -p hytte-services hooks::tests::nonzero_exit_logs_warn_with_outputs -- --nocapture`
Expected: FAIL — non-zero branch is currently a no-op.

- [ ] **Step 3: Implement the non-zero branch**

In `run_inner`, replace the `Ok(_) | Err(_) => { ... }` arm with:

```rust
        Ok(((sout, serr), Ok(status))) => {
            tracing::warn!(
                event,
                status = ?status,
                stdout = %String::from_utf8_lossy(&sout),
                stderr = %String::from_utf8_lossy(&serr),
                "hooks: script failed",
            );
        }
        Ok(((_sout, _serr), Err(e))) => {
            tracing::warn!(event, error = %e, "hooks: wait failed");
        }
        Err(_) => {
            // timeout — handled in a later task
        }
```

- [ ] **Step 4: Run the test, verify it passes**

Run: `cargo test -p hytte-services hooks::tests::nonzero_exit_logs_warn_with_outputs -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Run all hook tests**

Run: `cargo test -p hytte-services hooks:: -- --nocapture`
Expected: 3 passed.

- [ ] **Step 6: Commit**

```bash
git add crates/hytte-services/src/hooks.rs
git commit -m "feat(hooks): warn on non-zero exit with status + outputs"
```

---

## Task 5: Non-executable file → WARN, never spawn

**Files:**

- Modify: `crates/hytte-services/src/hooks.rs`

- [ ] **Step 1: Write the failing test**

Append to `mod tests`:

```rust
    #[tokio::test(flavor = "current_thread")]
    async fn non_executable_warns_and_does_not_run() {
        let home = TestHome::new();
        let sentinel = home.root.join("sentinel");
        let body = format!("#!/bin/sh\ntouch {}\n", sentinel.display());
        home.write_script("theme-changed", &body, 0o644); // no exec bit
        let (cap, _guard) = capture();

        super::run("theme-changed", &[]);

        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let events = cap.events.lock().unwrap().clone();
        assert!(
            events.iter().any(|e| e.level == tracing::Level::WARN
                && e.message.contains("not executable")),
            "expected WARN 'not executable', got: {:#?}",
            events,
        );
        assert!(!sentinel.exists(), "script must not have been executed");
    }
```

- [ ] **Step 2: Run the test, verify it fails**

Run: `cargo test -p hytte-services hooks::tests::non_executable_warns_and_does_not_run -- --nocapture`
Expected: FAIL — the script has no exec bit, so `cmd.spawn()` returns a permission error which currently logs `"hooks: spawn failed"`, not `"not executable"`. (Or, on some systems, the spawn does fail noisily; either way the message doesn't match.)

- [ ] **Step 3: Implement the exec-bit pre-check**

In `run_inner`, after the `if !meta.is_file()` check and before building `cmd`, insert:

```rust
    if meta.permissions().mode() & 0o111 == 0 {
        tracing::warn!(event, path = %path.display(), "hooks: script not executable");
        return;
    }
```

- [ ] **Step 4: Run the test, verify it passes**

Run: `cargo test -p hytte-services hooks::tests::non_executable_warns_and_does_not_run -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Run all hook tests**

Run: `cargo test -p hytte-services hooks:: -- --nocapture`
Expected: 4 passed.

- [ ] **Step 6: Commit**

```bash
git add crates/hytte-services/src/hooks.rs
git commit -m "feat(hooks): refuse non-executable scripts with WARN"
```

---

## Task 6: Timeout → kill child, WARN

**Files:**

- Modify: `crates/hytte-services/src/hooks.rs`

This task lowers `HOOK_TIMEOUT` to a faster value gated by `cfg(test)` so the test doesn't take 10s. The production const stays at 10s.

- [ ] **Step 1: Make the timeout test-tunable**

Replace the `HOOK_TIMEOUT` const at the top of `hooks.rs` with:

```rust
#[cfg(not(test))]
const HOOK_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(test)]
const HOOK_TIMEOUT: Duration = Duration::from_millis(500);
```

- [ ] **Step 2: Write the failing test**

Append to `mod tests`:

```rust
    #[tokio::test(flavor = "current_thread")]
    async fn timeout_kills_child_and_warns() {
        let home = TestHome::new();
        home.write_script("theme-changed", "#!/bin/sh\nsleep 30\n", 0o755);
        let (cap, _guard) = capture();

        let started = std::time::Instant::now();
        super::run("theme-changed", &[]);

        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            if cap.events.lock().unwrap().iter()
                .any(|e| e.level == tracing::Level::WARN
                    && e.message.contains("timed out")) { break; }
        }

        let elapsed = started.elapsed();
        assert!(elapsed < std::time::Duration::from_secs(5),
            "timeout should fire well before 10s; took {:?}", elapsed);

        let events = cap.events.lock().unwrap().clone();
        assert!(
            events.iter().any(|e| e.level == tracing::Level::WARN
                && e.message.contains("timed out")),
            "expected WARN 'timed out', got: {:#?}",
            events,
        );
    }
```

- [ ] **Step 3: Run the test, verify it fails**

Run: `cargo test -p hytte-services hooks::tests::timeout_kills_child_and_warns -- --nocapture`
Expected: FAIL — currently the timeout arm is a no-op and `child` keeps being awaited until it exits or the test itself times out.

Note: because `tokio::time::timeout` _cancels_ its inner future (which drops the `Child` and kills it on Linux via `kill_on_drop` if set), we need a) `kill_on_drop(true)` on the Command, b) explicit logging in the timeout arm.

- [ ] **Step 4: Implement kill-on-drop + timeout WARN**

In `run_inner`, find:

```rust
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env("TROLLSHELL_EVENT", event);
```

…and add `.kill_on_drop(true)` to the chain:

```rust
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .env("TROLLSHELL_EVENT", event);
```

Then replace the `Err(_) => { /* timeout — handled in a later task */ }` arm with:

```rust
        Err(_) => {
            tracing::warn!(event, "hooks: script timed out, killing");
            // Dropping `child` via end-of-scope triggers kill_on_drop.
        }
```

- [ ] **Step 5: Run the test, verify it passes**

Run: `cargo test -p hytte-services hooks::tests::timeout_kills_child_and_warns -- --nocapture`
Expected: PASS.

- [ ] **Step 6: Run all hook tests**

Run: `cargo test -p hytte-services hooks:: -- --nocapture`
Expected: 5 passed.

- [ ] **Step 7: Commit**

```bash
git add crates/hytte-services/src/hooks.rs
git commit -m "feat(hooks): enforce 10s timeout, kill child on drop"
```

---

## Task 7: Env vars passed through

Behavior: caller-supplied env (and `TROLLSHELL_EVENT`) reach the script.

**Files:**

- Modify: `crates/hytte-services/src/hooks.rs`

- [ ] **Step 1: Write the failing test**

Append to `mod tests`:

```rust
    #[tokio::test(flavor = "current_thread")]
    async fn env_vars_reach_script() {
        let home = TestHome::new();
        let sentinel = home.root.join("env-out");
        let body = format!(
            "#!/bin/sh\nprintf 'event=%s theme=%s' \"$TROLLSHELL_EVENT\" \"$TROLLSHELL_THEME\" > {}\n",
            sentinel.display(),
        );
        home.write_script("theme-changed", &body, 0o755);
        let (_cap, _guard) = capture();

        super::run("theme-changed", &[("TROLLSHELL_THEME", "dark")]);

        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            if sentinel.exists() { break; }
        }

        let contents = std::fs::read_to_string(&sentinel)
            .expect("script should have written sentinel");
        assert_eq!(contents, "event=theme-changed theme=dark");
    }
```

- [ ] **Step 2: Run the test, verify it passes**

Run: `cargo test -p hytte-services hooks::tests::env_vars_reach_script -- --nocapture`
Expected: PASS — env-var passthrough was implemented in Task 3 already; this test confirms it.

(If the test fails, the `TROLLSHELL_EVENT` line or the `for (k, v) in env` loop has been broken. Don't add new code; debug the existing implementation.)

- [ ] **Step 3: Run all hook tests**

Run: `cargo test -p hytte-services hooks:: -- --nocapture`
Expected: 6 passed.

- [ ] **Step 4: Commit**

```bash
git add crates/hytte-services/src/hooks.rs
git commit -m "test(hooks): assert env vars reach the script"
```

---

## Task 8: Wire `theme::set` to fire `theme-changed`

**Files:**

- Modify: `crates/hytte-services/src/theme.rs`

- [ ] **Step 1: Read the current `theme::set`**

Run: `sed -n '96,124p' crates/hytte-services/src/theme.rs`
Expected: prints the existing `pub fn set(theme: Theme)` from the doc comment through the closing `}` of the function. Confirm the body ends right after `update_qtct_conf("qt6ct", theme)`.

- [ ] **Step 2: Add the hooks call at the bottom of `set`**

In `crates/hytte-services/src/theme.rs`, replace:

```rust
    if let Err(e) = update_qtct_conf("qt6ct", theme) {
        tracing::warn!(error = %e, "theme: qt6ct.conf update failed");
    }
}
```

…with:

```rust
    if let Err(e) = update_qtct_conf("qt6ct", theme) {
        tracing::warn!(error = %e, "theme: qt6ct.conf update failed");
    }

    crate::hooks::run(
        "theme-changed",
        &[(
            "TROLLSHELL_THEME",
            match theme {
                Theme::Light => "light",
                Theme::Dark => "dark",
            },
        )],
    );
}
```

- [ ] **Step 3: Build and run all hytte-services tests**

Run: `cargo test -p hytte-services`
Expected: builds clean; all hook tests + all existing theme tests pass.

- [ ] **Step 4: Build the trollshell binary**

Run: `cargo build -p trollshell`
Expected: clean build (no clippy warnings; the workspace `[lints]` is strict).

- [ ] **Step 5: Run clippy across the workspace**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: zero warnings, exit 0.

- [ ] **Step 6: Commit**

```bash
git add crates/hytte-services/src/theme.rs
git commit -m "feat(theme): fire theme-changed hook after toolkit fan-out"
```

---

## Task 9: Manual smoke test (no code changes)

- [ ] **Step 1: Drop a hook script**

Outside `cargo`, in a separate shell:

```bash
mkdir -p ~/.config/trollshell/hooks
cat > ~/.config/trollshell/hooks/theme-changed <<'EOF'
#!/bin/sh
echo "theme-changed: TROLLSHELL_THEME=$TROLLSHELL_THEME at $(date)" >> /tmp/trollshell-hook.log
EOF
chmod +x ~/.config/trollshell/hooks/theme-changed
```

- [ ] **Step 2: Run trollshell and toggle the theme**

```bash
RUST_LOG=hytte_services=info,trollshell=info cargo run -p trollshell
```

Toggle the theme in the settings panel light → dark and back. Watch stdout for `hooks: ran` info events.

- [ ] **Step 3: Verify the hook ran**

```bash
cat /tmp/trollshell-hook.log
```

Expected: at least one line per toggle, with `TROLLSHELL_THEME=dark` and `=light` matching the toggles.

- [ ] **Step 4: Clean up smoke artifacts**

```bash
rm /tmp/trollshell-hook.log
# (Leave ~/.config/trollshell/hooks/theme-changed in place if you want it; it's the user's now.)
```

No commit — this task is verification only.

---

## Self-review checklist (already run)

- [x] Spec coverage: every section of the spec maps to a task. The failure-mode table maps to tests in tasks 2/4/5/6 and to implementation branches in tasks 2/3/4/5/6. Env vars: task 7. Wire-up: task 8.
- [x] No placeholders: every code step has the actual code; every command step has the exact `cargo` invocation and expected outcome.
- [x] Type/name consistency: `run`, `run_inner`, `resolve_path`, `spawn_task`, `HOOK_TIMEOUT`, `Captured`, `CapturedEvent`, `TestHome` are all defined in task 1 and used consistently in later tasks.
- [x] One spec callout: the spec's `Mutex`-around-`$HOME` test pattern lands in task 1's `TestHome` helper. The `tracing-subscriber` dev-dep is added in task 1 step 1 (the spec flagged "if not already there" — it isn't, so it gets added).
