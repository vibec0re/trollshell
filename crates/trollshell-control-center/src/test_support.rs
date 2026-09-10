//! Shared `gtk_tests` capture helper (#1017 review, LOW 3).
//!
//! `main.rs`, `ai_keys_tab.rs` and `plugins_tab.rs` each carried a
//! byte-identical `CapturedLog`/`captured_logs` pair — a `tracing` writer
//! that collects emitted lines in memory so a test can count them instead of
//! inferring them from widget state. This is the one definition all three
//! `gtk_tests` modules call; `plugins_tab`'s own `captured_transition_logs`
//! wraps [`captured_logs`] with its `"ListPlugins"` filter rather than
//! keeping a fourth copy of the capture plumbing itself.
//!
//! Only compiled alongside the `gtk_tests` modules it serves — same gate
//! (`test` + `system-tests`) as those.

use std::io::Write;
use std::sync::{Arc, Mutex};

/// A `tracing` writer that collects every emitted line in memory, so a test
/// can count log lines rather than infer them from state.
///
/// Hand-rolled rather than reached for from `tracing-subscriber`'s test
/// helpers because `TestWriter` goes to the captured stdout, which a test
/// cannot read back.
#[derive(Clone, Default)]
pub(crate) struct CapturedLog(Arc<Mutex<Vec<u8>>>);

impl Write for CapturedLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("the capture buffer is never held across a panic")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedLog {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Run `body` with an `INFO` subscriber installed for this thread, and
/// return every line it emitted.
///
/// `set_default` is thread-local, so this neither needs nor disturbs a
/// global subscriber, and `#[gtk::test]` runs the body on the same thread.
pub(crate) fn captured_logs(body: impl FnOnce()) -> Vec<String> {
    let buffer = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(CapturedLog(buffer.clone()))
        .with_max_level(tracing::Level::INFO)
        .finish();
    {
        let _guard = tracing::subscriber::set_default(subscriber);
        body();
    }
    let bytes = buffer.lock().expect("no panic while capturing").clone();
    String::from_utf8_lossy(&bytes)
        .lines()
        .map(str::to_owned)
        .collect()
}
