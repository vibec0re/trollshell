//! The fake `host.sock` harness, shared by `tests/fake_socket.rs` (one round
//! trip) and `tests/poll_task.rs` (the loop around it).
//!
//! A **real** `UnixListener` in a `TempDir` speaking the recorded JSON lines
//! under `tests/fixtures/`. Nothing about the client is stubbed: it connects,
//! writes one line, reads one line, exactly as it would against `hive-c0re`.
//! What is stubbed is only the daemon's answers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};

/// One recorded `HostResponse` line from `tests/fixtures/`.
///
/// # Panics
/// If the fixture is missing — a typo'd name should fail loudly, not silently
/// serve a bare success.
#[must_use]
pub fn fixture(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing fixture {} ({e})", path.display()))
        .trim()
        .to_owned()
}

/// A `host.sock` stand-in. Records every request line it is sent, and answers
/// each from a `cmd` → reply table.
pub struct FakeHive {
    /// Kept alive so the socket's directory outlives the listener.
    _dir: tempfile::TempDir,
    path: PathBuf,
    seen: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeHive {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeHive {
    /// Bind at `<tmp>/host.sock` and serve `replies`, keyed by the request's
    /// `cmd` tag. A `cmd` with no entry gets a bare success.
    ///
    /// # Panics
    /// If the tempdir or the bind fails — there is no useful test to run then.
    #[must_use]
    pub fn serve(replies: HashMap<&'static str, String>) -> Self {
        let dir = tempfile::tempdir().expect("a tempdir");
        let path = dir.path().join("host.sock");
        let listener = UnixListener::bind(&path).expect("bind the fake host.sock");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let task = tokio::spawn(accept_loop(listener, replies, Arc::clone(&seen)));
        Self {
            _dir: dir,
            path,
            seen,
            task,
        }
    }

    /// The socket path a client dials.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every request line the fake has been sent, in order.
    ///
    /// # Panics
    /// Only if a test thread panicked while holding the recorder.
    #[must_use]
    pub fn seen(&self) -> Vec<String> {
        self.seen
            .lock()
            .expect("the recorder is never poisoned")
            .clone()
    }
}

/// Serve connections until the listener dies.
pub async fn accept_loop(
    listener: UnixListener,
    replies: HashMap<&'static str, String>,
    seen: Arc<Mutex<Vec<String>>>,
) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        // One request/response per connection, exactly like the real daemon
        // (and exactly what the client's one-connection-per-request model
        // expects).
        serve_one(stream, &replies, &seen).await;
    }
}

async fn serve_one(
    stream: UnixStream,
    replies: &HashMap<&'static str, String>,
    seen: &Arc<Mutex<Vec<String>>>,
) {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    if reader.read_line(&mut line).await.is_err() {
        return;
    }
    let line = line.trim().to_owned();
    let cmd = serde_json::from_str::<serde_json::Value>(&line)
        .ok()
        .and_then(|v| v.get("cmd").and_then(|c| c.as_str()).map(str::to_owned))
        .unwrap_or_default();
    seen.lock()
        .expect("the recorder is never poisoned")
        .push(line);

    let reply = replies
        .get(cmd.as_str())
        .cloned()
        .unwrap_or_else(|| r#"{"version":1,"ok":true}"#.to_owned());
    let _ = write.write_all(format!("{reply}\n").as_bytes()).await;
    let _ = write.flush().await;
}

/// Build a `cmd` → reply table.
#[must_use]
pub fn replies(pairs: &[(&'static str, &str)]) -> HashMap<&'static str, String> {
    pairs
        .iter()
        .map(|(cmd, body)| (*cmd, (*body).to_owned()))
        .collect()
}
