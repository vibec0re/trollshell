//! User-authored hook scripts fired on trollshell events.
//!
//! Resolves `$HOME/.config/trollshell/hooks/<event>` and spawns it with
//! caller-supplied env vars plus `TROLLSHELL_EVENT`. Fire-and-forget from
//! the caller's POV: all outcomes go to `tracing`. See
//! `docs/superpowers/specs/2026-05-05-settings-hooks-design.md`.

#[allow(unused_imports)]
use std::os::unix::fs::PermissionsExt;
#[allow(unused_imports)]
use std::path::PathBuf;
use std::time::Duration;

#[allow(dead_code)]
const HOOK_TIMEOUT: Duration = Duration::from_secs(10);

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
    let _meta = match tokio::fs::metadata(&path).await {
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
    let _ = env; // remaining branches arrive in later tasks
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
}
