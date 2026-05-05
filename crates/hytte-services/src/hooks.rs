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
    let _ = (event, env);
    // TODO(impl in later tasks)
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
        /// Run `f` with `$HOME` temporarily set to a fresh temp directory.
        ///
        /// `temp_env::with_var` serializes env mutation and restores the
        /// previous value on return or panic — replacing the manual
        /// `HOME_LOCK` + `unsafe set_var` pattern from the original design.
        pub fn with<F, R>(f: F) -> R
        where
            F: FnOnce(&Self) -> R,
        {
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir()
                .join(format!("hytte-hooks-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            let home = Self { root: root.clone() };
            let result = temp_env::with_var("HOME", Some(&root), || f(&home));
            let _ = std::fs::remove_dir_all(&root);
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
}
