//! Process-wide multi-thread tokio runtime, initialized lazily on first
//! `handle()` call. Services use this `Handle` to spawn their I/O tasks.

use std::sync::OnceLock;
use tokio::runtime::{Handle, Runtime};

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

/// Returns a stable reference to the process-wide tokio runtime handle.
///
/// The runtime is built on first call. All subsequent calls return a handle
/// to the same runtime.
#[must_use]
pub fn handle() -> &'static Handle {
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_name("hytte-tokio")
                .thread_stack_size(8 * 1024 * 1024) // 8 MB; default 2 MB is tight in debug
                .build()
                .expect("failed to build hytte tokio runtime")
        })
        .handle()
}
