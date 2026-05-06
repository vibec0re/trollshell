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
        use tokio::io::AsyncReadExt;
        let read_one = |stream: Option<tokio::process::ChildStdout>| async move {
            let mut buf = Vec::new();
            if let Some(mut s) = stream {
                let _ = s.read_to_end(&mut buf).await;
            }
            buf
        };
        let read_one_err = |stream: Option<tokio::process::ChildStderr>| async move {
            let mut buf = Vec::new();
            if let Some(mut s) = stream {
                let _ = s.read_to_end(&mut buf).await;
            }
            buf
        };
        tokio::join!(read_one(stdout), read_one_err(stderr))
    };
    let wait = async {
        tokio::join!(read_outputs, child.wait())
    };

    match tokio::time::timeout(HOOK_TIMEOUT, wait).await {
        Ok(((sout, serr), Ok(status))) if status.success() => {
            tracing::info!(event, "hooks: ran");
            if !sout.is_empty() {
                tracing::info!(
                    event,
                    stdout = %String::from_utf8_lossy(&sout),
                    "hooks: stdout",
                );
            }
            if !serr.is_empty() {
                tracing::info!(
                    event,
                    stderr = %String::from_utf8_lossy(&serr),
                    "hooks: stderr",
                );
            }
        }
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
            tracing::warn!(event, "hooks: script timed out, killing");
        }
    }
}

fn resolve_path(event: &str) -> Option<PathBuf> {
    let Some(home) = std::env::var_os("HOME") else {
        tracing::warn!(event, "hooks: $HOME not set");
        return None;
    };
    Some(PathBuf::from(home).join(".config/trollshell/hooks").join(event))
}

#[cfg(test)]
#[allow(dead_code)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;
    use tracing::{Event, Subscriber};
    use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
    use tracing_subscriber::Registry;

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
            let root = std::env::temp_dir()
                .join(format!("hytte-hooks-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            let cleanup = root.clone();
            let home = TestHome { root: root.clone() };
            let result = temp_env::async_with_vars(
                [("HOME", Some(root.into_os_string()))],
                f(home),
            )
            .await;
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
                self.fields.insert(field.name().to_string(), value.to_string());
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

            // Wait up to 2s for completion + log emission.
            for _ in 0..40 {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                if cap
                    .events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|e| e.level == tracing::Level::INFO)
                {
                    break;
                }
            }

            let events = cap.events.lock().unwrap().clone();
            assert!(
                events
                    .iter()
                    .any(|e| e.level == tracing::Level::INFO && e.message.contains("ran")),
                "expected an INFO 'ran' event, got: {events:#?}",
            );
            assert!(
                events.iter().any(|e| e.level == tracing::Level::INFO
                    && e.fields.get("stdout").is_some_and(|s| s.contains("hi"))),
                "expected an INFO event with stdout=hi, got: {events:#?}",
            );
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

            for _ in 0..40 {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                if cap
                    .events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|e| e.level == tracing::Level::WARN)
                {
                    break;
                }
            }

            let events = cap.events.lock().unwrap().clone();
            let warn = events
                .iter()
                .find(|e| e.level == tracing::Level::WARN)
                .unwrap_or_else(|| panic!("expected a WARN event, got: {events:#?}"));
            assert!(
                warn.message.contains("failed"),
                "warn msg: {:?}",
                warn.message,
            );
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

            tokio::time::sleep(std::time::Duration::from_millis(200)).await;

            let events = cap.events.lock().unwrap().clone();
            assert!(
                events.iter().any(|e| e.level == tracing::Level::WARN
                    && e.message.contains("not executable")),
                "expected WARN 'not executable', got: {events:#?}",
            );
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

            for _ in 0..40 {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                if cap.events.lock().unwrap().iter().any(|e| {
                    e.level == tracing::Level::WARN && e.message.contains("timed out")
                }) {
                    break;
                }
            }

            let elapsed = started.elapsed();
            assert!(
                elapsed < std::time::Duration::from_secs(5),
                "timeout should fire well before 10s; took {elapsed:?}",
            );

            let events = cap.events.lock().unwrap().clone();
            assert!(
                events.iter().any(|e| e.level == tracing::Level::WARN
                    && e.message.contains("timed out")),
                "expected WARN 'timed out', got: {events:#?}",
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

            // Spawn happens on a background task — give it a tick to land.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;

            let events = cap.events.lock().unwrap().clone();
            assert!(
                events.iter().any(|e| e.level == tracing::Level::DEBUG
                    && e.message.contains("no script")),
                "expected a DEBUG 'no script' event, got: {events:#?}",
            );
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

            for _ in 0..40 {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                if sentinel.exists() {
                    break;
                }
            }

            let contents = std::fs::read_to_string(&sentinel)
                .expect("script should have written sentinel");
            assert_eq!(contents, "event=theme-changed theme=dark");
        })
        .await;
    }
}
