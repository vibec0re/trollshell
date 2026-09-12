//! A **scripted** `host.sock`: a real `UnixListener` in a `TempDir` answering
//! from a queue, so a test can make the hive change its mind between polls.
//!
//! The shape is `hytte-plugin-agents/tests/fake/mod.rs`'s — a real socket, the
//! real client, only the daemon's answers stubbed — with one difference that
//! is the whole reason it is not shared: that one serves a fixed `cmd` → reply
//! table, and what this window's tests have to exercise is a status *change*
//! over a sequence of polls.
//!
//! Nothing here reaches the network or the real `/run/hyperhive/host.sock`.

#![allow(dead_code, reason = "each test binary uses a different part of this")]

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};

/// A `host.sock` stand-in. Records every request line it is sent, and answers
/// each `agent_status` from a script; every other verb gets a bare success.
pub struct FakeHive {
    /// Kept alive so the socket's directory outlives the listener.
    _dir: tempfile::TempDir,
    path: PathBuf,
    seen: Arc<Mutex<Vec<String>>>,
    /// `cmd` → the reason it is refused with `ok:false`. See
    /// [`FakeHive::refusing`].
    refusals: Table,
    /// `cmd` → a verbatim reply line, for the verbs a test wants to script by
    /// hand (see [`FakeHive::with_urls`]).
    replies: Table,
    task: tokio::task::JoinHandle<()>,
}

/// A `cmd` → string table the serving task shares with the handle.
type Table = Arc<Mutex<HashMap<String, String>>>;

impl Drop for FakeHive {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeHive {
    /// Bind at `<tmp>/host.sock` and answer `agent_status` from `script`, in
    /// order; the **last** entry repeats forever, so a test states only the
    /// transitions it cares about.
    #[must_use]
    pub fn script(script: &[&str]) -> Self {
        let dir = tempfile::tempdir().expect("a tempdir");
        let path = dir.path().join("host.sock");
        let listener = UnixListener::bind(&path).expect("bind the fake host.sock");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let refusals: Table = Arc::new(Mutex::new(HashMap::new()));
        let replies: Table = Arc::new(Mutex::new(HashMap::new()));
        let queue: VecDeque<String> = script.iter().map(|s| (*s).to_owned()).collect();
        let task = tokio::spawn(accept_loop(
            listener,
            Arc::new(Mutex::new(queue)),
            Arc::clone(&seen),
            Arc::clone(&refusals),
            Arc::clone(&replies),
        ));
        Self {
            _dir: dir,
            path,
            seen,
            refusals,
            replies,
            task,
        }
    }

    /// Answer the `urls` verb with a real `HiveUrls`.
    ///
    /// Without this the fake answers `urls` with a bare success carrying **no
    /// urls field**, which is what a hive that cannot answer looks like — and
    /// since #1130 L6 that is retried rather than given up on, so a test that
    /// wants the once-and-done path has to say so.
    #[must_use]
    pub fn with_urls(self, domain: &str, home: &str) -> Self {
        self.replies
            .lock()
            .expect("the reply table is never poisoned")
            .insert(
                "urls".to_owned(),
                format!(
                    r#"{{"version":1,"ok":true,"urls":{{"domain":"{domain}","home":"{home}"}}}}"#
                ),
            );
        self
    }

    /// Answer `cmd` with `{"ok":false,"error":"<reason>"}` — **a hive that
    /// says no**, which nothing could express before #1130's review: the fake
    /// answered every non-`agent_status` verb with a bare success, so the
    /// whole refusal path was unreachable from a test.
    ///
    /// Takes `self` so a test reads as one sentence
    /// (`FakeHive::script(…).refusing("set_paused", "agent busy")`).
    #[must_use]
    pub fn refusing(self, cmd: &str, reason: &str) -> Self {
        self.refusals
            .lock()
            .expect("the refusal table is never poisoned")
            .insert(cmd.to_owned(), reason.to_owned());
        self
    }

    /// The socket path a client dials.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every request line the fake has been sent, in order.
    #[must_use]
    pub fn seen(&self) -> Vec<String> {
        self.seen
            .lock()
            .expect("the recorder is never poisoned")
            .clone()
    }

    /// Only the lines that are not the status poll — i.e. what a button did.
    ///
    /// `pending` rides the same poll as `agent_status` since #1141 and is
    /// filtered out here for the same reason: it is the window asking, not
    /// the operator clicking.
    #[must_use]
    pub fn writes(&self) -> Vec<String> {
        self.seen()
            .into_iter()
            .filter(|l| {
                !l.contains("\"agent_status\"")
                    && !l.contains("\"urls\"")
                    && !l.contains("\"pending\"")
            })
            .collect()
    }
}

async fn accept_loop(
    listener: UnixListener,
    script: Arc<Mutex<VecDeque<String>>>,
    seen: Arc<Mutex<Vec<String>>>,
    refusals: Table,
    replies: Table,
) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        // One request/response per connection, exactly like the real daemon
        // and what the client's one-connection-per-request model expects.
        serve_one(stream, &script, &seen, &refusals, &replies).await;
    }
}

async fn serve_one(
    stream: UnixStream,
    script: &Arc<Mutex<VecDeque<String>>>,
    seen: &Arc<Mutex<Vec<String>>>,
    refusals: &Table,
    replies: &Table,
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

    let refused = refusals
        .lock()
        .expect("the refusal table is never poisoned")
        .get(&cmd)
        .cloned();

    let scripted = replies
        .lock()
        .expect("the reply table is never poisoned")
        .get(&cmd)
        .cloned();

    let reply = if let Some(reason) = refused {
        // The hive's own shape for a no: `ok:false` plus a sentence
        // (`hive-host-sock`'s `HostResponse`). `client::request` turns it into
        // `HiveError::Refused` carrying exactly this string.
        let escaped = reason.replace('\\', r"\\").replace('"', r#"\""#);
        format!(r#"{{"version":1,"ok":false,"error":"{escaped}"}}"#)
    } else if let Some(line) = scripted {
        line
    } else if cmd == "agent_status" {
        let mut q = script.lock().expect("the script is never poisoned");
        if q.len() > 1 {
            q.pop_front().unwrap_or_default()
        } else {
            q.front().cloned().unwrap_or_default()
        }
    } else {
        r#"{"version":1,"ok":true}"#.to_owned()
    };
    let _ = write.write_all(format!("{reply}\n").as_bytes()).await;
    let _ = write.flush().await;
}

/// One `agent_status` answer carrying a single row, spelled by hand so a test
/// reads as the hive's own JSON rather than as a builder call.
#[must_use]
pub fn roster(row: &str) -> String {
    format!(r#"{{"version":1,"ok":true,"agent_statuses":[{row}]}}"#)
}
