//! Clipboard history reader for `cliphist` (storage layer) + `wl-clipboard`.
//!
//! Capture happens out-of-process: two systemd user units run
//! `wl-paste --watch cliphist store` (one for text, one for images) so every
//! clip the user makes ends up in cliphist's on-disk database. See
//! `etc/cliphist/README.md` for the install + verification recipe.
//!
//! This service is the read+paste side. It exposes:
//!
//! * [`history()`] — `Signal<Vec<ClipEntry>>`, the most-recent N entries
//!   from `cliphist list`. Empty until [`refresh()`] is called.
//! * `paste_entry(id)` — fire-and-forget. Pipes `cliphist decode <id>`
//!   into `wl-copy` so the entry becomes the active clipboard payload.
//! * [`refresh()`] — re-runs `cliphist list` and updates the signal.
//!
//! # Refresh semantics
//!
//! Refresh-on-page-open: the drawer page calls [`refresh()`] when it
//! mounts. We don't poll in the background — clipboard history is only
//! ever consumed by the user opening the drawer page, and `cliphist list`
//! is fast enough that on-demand is fine.
//!
//! # Scope (v1)
//!
//! Delete by id is supported via [`delete`]. Implementation pipes the
//! bare id into `cliphist delete` — one subprocess call per delete (see
//! `run_delete_by_id` for why a bare id is sufficient; #742 has the
//! full writeup).
//!
//! No clip pinning, no search/filter UI, no multi-select, no rich-format
//! paste. The page is a plain history list with click-to-paste.

use futures_signals::signal::{Mutable, Signal};
use hytte_reactive::{Service, registry, runtime};
use std::process::Stdio;

// ── Public data types ────────────────────────────────────────────────────────

/// One clipboard history entry as reported by `cliphist list`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipEntry {
    /// cliphist's row id — the integer at the start of each list line.
    /// Stable for the lifetime of an entry; used by `paste_entry`.
    pub id: u64,
    /// Truncated preview suitable for an `AdwActionRow` title. For images
    /// this is cliphist's `[[ binary data … ]]` placeholder, normalized to
    /// a short "Image" label upstream of the parser.
    pub preview: String,
    /// Whether the entry is text or a binary image blob.
    pub kind: ClipKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipKind {
    Text,
    Image,
}

/// Cap on entries surfaced to the UI. cliphist itself can hold many more,
/// but the drawer page is short and a long list balloons the page height.
const MAX_ENTRIES: usize = 50;

/// Cap on individual preview length; mirrors what `cliphist list` itself
/// does internally (~100 chars), but tighter so very-wide clips don't
/// push the modal surface wide.
const PREVIEW_MAX: usize = 80;

// ── Service handle ───────────────────────────────────────────────────────────

#[doc(hidden)]
pub struct ClipboardHandles {
    pub(crate) history: Mutable<Vec<ClipEntry>>,
}

impl Default for ClipboardHandles {
    fn default() -> Self {
        Self {
            history: Mutable::new(Vec::new()),
        }
    }
}

/// Clipboard service marker. Pass to `App::with` to register. No background
/// loop is started — `refresh()` is the only path that mutates state.
pub struct ClipboardService;

impl Service for ClipboardService {
    type Handles = ClipboardHandles;

    fn start(self, _rt: &tokio::runtime::Handle) -> Self::Handles {
        ClipboardHandles::default()
    }
}

#[must_use]
pub fn service() -> ClipboardService {
    ClipboardService
}

// ── Public API ───────────────────────────────────────────────────────────────

/// Signal of the cached clipboard history. Empty until [`refresh()`] runs.
pub fn history() -> impl Signal<Item = Vec<ClipEntry>> {
    registry::with(|r| {
        r.get::<ClipboardHandles>()
            .expect("clipboard::service() not registered")
            .history
            .signal_cloned()
    })
}

/// Re-fetch `cliphist list` and update the [`history()`] signal. Intended
/// to be called when the drawer's clipboard page becomes visible. Errors
/// (cliphist not installed, exec failure) are logged at debug and the
/// signal is set to empty so the page shows the empty state instead of a
/// stale list.
pub fn refresh() {
    // Capture the writer outside the spawned task so we can early-return
    // with a warn if the service isn't registered without crashing.
    let writer = registry::with(|r| r.get::<ClipboardHandles>().map(|h| h.history.clone()));
    let Some(writer) = writer else {
        tracing::warn!("clipboard::refresh: service not registered");
        return;
    };

    runtime::handle().spawn_blocking(move || reload_into(&writer));
}

/// Re-read `cliphist list` and publish it if it differs from the cached
/// snapshot. Blocking — always call from `spawn_blocking`.
///
/// Shared by [`refresh()`] and [`delete()`]'s post-delete reconcile so both
/// paths dedup identically.
fn reload_into(writer: &Mutable<Vec<ClipEntry>>) {
    let entries = run_cliphist_list();
    // PartialEq dedup: only write when the snapshot actually differs
    // so signal subscribers don't rebuild rows for an identical list.
    let changed = {
        let cur = writer.lock_ref();
        *cur != entries
    };
    if changed {
        writer.set(entries);
    }
}

/// Re-paste a history entry by id. Pipes `cliphist decode <id>` into
/// `wl-copy`. Fire-and-forget; failure is logged at warn.
pub fn paste_entry(id: u64) {
    runtime::handle().spawn_blocking(move || {
        if let Err(e) = run_decode_to_wlcopy(id) {
            tracing::warn!(id, error = %e, "clipboard: paste_entry failed");
        }
    });
}

/// Delete a history entry by id. Pipes the bare id into `cliphist
/// delete` (see `run_delete_by_id`), and updates the [`history()`]
/// signal so the row disappears from an open drawer.
///
/// # Why this doesn't just call [`refresh()`]
///
/// It used to, and that was a bug: `refresh()` spawns its own blocking
/// task, so a single `cliphist list` raced the delete task's
/// subprocess(es). At the time this was fixed, deleting still ran
/// `list` *then* `delete` (#742 later collapsed that to the single
/// `delete` call `run_delete_by_id` makes today), and the shorter
/// one-subprocess `refresh()` task essentially always won that race, so
/// the emitted snapshot was the pre-delete one and the deleted row
/// stayed on screen until the drawer was closed and reopened. Measured
/// against real cliphist 0.7.0 it lost 20/20. The two-phase design below
/// doesn't depend on delete's subprocess count — it fixes the race by
/// not running a concurrent `refresh()` at all.
///
/// So the update is now two-phase:
///
/// 1. **Optimistic** — prune `id` from the cached snapshot synchronously,
///    on the caller's (GTK) thread, so the row vanishes on click.
/// 2. **Authoritative** — after the delete subprocess exits, re-read
///    `cliphist list` *in the same blocking task* and publish that. This
///    reflects ground truth, and self-heals: if the delete actually
///    failed, the entry reappears rather than staying optimistically gone.
///
/// Phase 1 is safe to do from a GTK callback: [`bind`](hytte_reactive::bind())
/// drives subscribers from a `glib::MainContext` task, so `set()` wakes the
/// apply-loop rather than polling it inline — the row rebuild lands on a
/// later main-loop iteration, after the click handler has returned.
///
/// Fire-and-forget; failures are logged at warn.
pub fn delete(id: u64) {
    // The registry is thread-local to the GTK thread. If it's somehow absent
    // the delete must still happen — only the snapshot updates are skipped —
    // so this is deliberately not an early return.
    let writer = registry::with(|r| r.get::<ClipboardHandles>().map(|h| h.history.clone()));
    if writer.is_none() {
        tracing::warn!("clipboard::delete: service not registered; deleting without a UI update");
    }

    // Phase 1: optimistic prune. Only emit when the id was actually present,
    // so a stale/duplicate delete doesn't rebuild every row for nothing.
    if let Some(writer) = writer.as_ref() {
        let pruned = {
            let cur = writer.lock_ref();
            cur.iter().any(|e| e.id == id).then(|| without_id(&cur, id))
        };
        if let Some(pruned) = pruned {
            writer.set(pruned);
        }
    }

    // Phase 2: authoritative reload, sequenced *after* the delete subprocess.
    runtime::handle().spawn_blocking(move || {
        if let Err(e) = run_delete_by_id(id) {
            tracing::warn!(id, error = %e, "clipboard: delete failed");
        }
        if let Some(writer) = writer {
            reload_into(&writer);
        }
    });
}

/// The snapshot to show immediately after the user deletes `id`, before
/// cliphist has been re-read: every other entry, in order.
fn without_id(entries: &[ClipEntry], id: u64) -> Vec<ClipEntry> {
    entries.iter().filter(|e| e.id != id).cloned().collect()
}

// ── Subprocess helpers ───────────────────────────────────────────────────────

fn run_cliphist_list() -> Vec<ClipEntry> {
    let output = match std::process::Command::new("cliphist")
        .arg("list")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
    {
        Ok(o) => o,
        Err(e) => {
            // ENOENT ⇒ cliphist not installed; treat as empty history.
            tracing::debug!(error = %e, "clipboard: spawn cliphist list failed");
            return Vec::new();
        }
    };

    if !output.status.success() {
        tracing::debug!(status = ?output.status, "clipboard: cliphist list non-zero");
        return Vec::new();
    }

    parse_list(&String::from_utf8_lossy(&output.stdout))
}

/// Parse `cliphist list` output. Each line is `<id>\t<preview>`. Image
/// previews look like `[[ binary data 12.3 KiB png ]]`; we map those to
/// [`ClipKind::Image`] with a short `"Image"` preview. Anything else is
/// text, truncated to [`PREVIEW_MAX`].
fn parse_list(text: &str) -> Vec<ClipEntry> {
    let mut out = Vec::with_capacity(MAX_ENTRIES);
    for line in text.lines().take(MAX_ENTRIES) {
        let Some((id_part, rest)) = line.split_once('\t') else {
            continue;
        };
        let Ok(id) = id_part.trim().parse::<u64>() else {
            continue;
        };
        let (kind, preview) = if is_image_preview(rest) {
            (ClipKind::Image, image_label(rest))
        } else {
            (ClipKind::Text, truncate(rest, PREVIEW_MAX))
        };
        // Skip blank rows: cliphist occasionally emits an id with an empty
        // preview, which would render as a blank clickable row in the UI.
        if preview.is_empty() {
            continue;
        }
        out.push(ClipEntry { id, preview, kind });
    }
    out
}

/// cliphist marks binary entries with a `[[ binary data … ]]` preview.
/// Match the canonical double-bracket prefix only — `[binary…` (single
/// bracket) is not something cliphist emits and would false-positive on
/// legitimate text starting with that string.
fn is_image_preview(preview: &str) -> bool {
    preview.trim_start().starts_with("[[ binary")
}

/// Compact image preview into `"Image (12.3 KiB png)"` when the size +
/// type can be teased out of cliphist's bracketed string; otherwise just
/// `"Image"`. Best-effort — if cliphist changes its format we fall back.
fn image_label(preview: &str) -> String {
    // Typical: "[[ binary data 12.3 KiB png ]]"
    let inside = preview.trim_start_matches('[').trim_end_matches(']').trim();
    let Some(rest) = inside.strip_prefix("binary data") else {
        return "Image".to_string();
    };
    let rest = rest.trim();
    if rest.is_empty() {
        "Image".to_string()
    } else {
        format!("Image ({rest})")
    }
}

fn truncate(s: &str, max: usize) -> String {
    // Char-boundary safe; appends an ellipsis when we cut.
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('\u{2026}');
    out
}
/// Upper bound on the bytes read back from `cliphist decode`. cliphist's
/// own per-item store cap defaults to 5 MB, so 64 MiB is generous; it only
/// exists so a misbehaving decoder cannot grow this buffer without limit.
const DECODE_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// How long `cliphist decode` may take before it is killed.
const DECODE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// What to do with the outcome of `cliphist decode`.
#[derive(Debug, PartialEq, Eq)]
enum DecodeVerdict {
    /// Decode succeeded with a payload: hand it to `wl-copy`.
    Copy,
    /// Decode exited cleanly but produced nothing; copying would blank
    /// the live clipboard.
    Empty,
    /// Decode failed: a non-zero exit, or a signal it was not killed by for
    /// overflowing.
    Failed,
    /// Output went past the cap ([`DECODE_MAX_BYTES`]); it is truncated, so do
    /// not copy. Whatever the decoder's exit status says: the paste path stops
    /// reading and kills a decoder that overflows, so its status is then a
    /// signal of ours, not a failure of its own.
    TooLarge,
}

/// Pure decision for the paste path: only a successful, non-empty,
/// untruncated decode may reach `wl-copy`. A stale or pruned id makes
/// `cliphist decode` exit non-zero with empty stdout, and `wl-copy` on
/// empty stdin would replace the user's live clipboard with nothing.
///
/// Overflow is checked **before** the exit status. Once a decoder writes past
/// `max_bytes`, [`run_decode_to_wlcopy_with`] stops reading (closing the pipe)
/// and kills it, so its status is a `SIGPIPE` or a `SIGKILL` of ours, not a
/// verdict of its own. Checked after it, that overflow was reported as
/// "exited" (`Failed`), and `TooLarge` was reachable only by a decoder that
/// had finished before either landed (#1559).
fn decode_verdict(success: bool, output_len: usize, max_bytes: u64) -> DecodeVerdict {
    if output_len as u64 > max_bytes {
        DecodeVerdict::TooLarge
    } else if !success {
        DecodeVerdict::Failed
    } else if output_len == 0 {
        DecodeVerdict::Empty
    } else {
        DecodeVerdict::Copy
    }
}

/// The two programs the paste path runs, and the bounds it runs the first
/// under. A parameter so the orchestration is testable against stand-in
/// scripts and with small bounds; [`PASTE`] is the real set.
struct PasteTools<'a> {
    cliphist: &'a str,
    wl_copy: &'a str,
    /// The most bytes `cliphist decode` may write before it is killed.
    max_bytes: u64,
    /// How long `cliphist decode` may run before it is killed.
    timeout: std::time::Duration,
}

/// The programs on `PATH`, under [`DECODE_MAX_BYTES`] and [`DECODE_TIMEOUT`].
const PASTE: PasteTools<'static> = PasteTools {
    cliphist: "cliphist",
    wl_copy: "wl-copy",
    max_bytes: DECODE_MAX_BYTES,
    timeout: DECODE_TIMEOUT,
};

/// Run `cliphist decode <id>` to completion (bounded in size and time),
/// then, only if it produced a payload, feed that to `wl-copy` on stdin.
fn run_decode_to_wlcopy(id: u64) -> anyhow::Result<()> {
    run_decode_to_wlcopy_with(&PASTE, id)
}

/// [`run_decode_to_wlcopy`] with the programs and bounds as a parameter.
fn run_decode_to_wlcopy_with(tools: &PasteTools<'_>, id: u64) -> anyhow::Result<()> {
    use std::io::{Read, Write};

    let max_bytes = tools.max_bytes;
    let mut decoder = std::process::Command::new(tools.cliphist)
        .arg("decode")
        .arg(id.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn cliphist decode: {e}"))?;

    let Some(stdout) = decoder.stdout.take() else {
        let _ = decoder.kill();
        let _ = decoder.wait();
        return Err(anyhow::anyhow!("cliphist decode: no stdout pipe"));
    };

    // Read on a helper thread so the timeout can kill a stalled decoder
    // (killing closes the pipe, which unblocks the reader with EOF).
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let r = stdout.take(max_bytes + 1).read_to_end(&mut buf);
        let _ = tx.send(r.map(|_| buf));
    });
    let read = rx.recv_timeout(tools.timeout);
    if read.is_err() || matches!(&read, Ok(Ok(b)) if b.len() as u64 > max_bytes) {
        let _ = decoder.kill();
    }
    let dec_status = decoder
        .wait()
        .map_err(|e| anyhow::anyhow!("wait cliphist decode: {e}"))?;
    let payload = match read {
        Ok(Ok(b)) => b,
        Ok(Err(e)) => return Err(anyhow::anyhow!("read cliphist decode: {e}")),
        Err(_) => return Err(anyhow::anyhow!("cliphist decode timed out")),
    };

    match decode_verdict(dec_status.success(), payload.len(), max_bytes) {
        DecodeVerdict::Copy => {}
        DecodeVerdict::Empty => {
            return Err(anyhow::anyhow!(
                "cliphist decode produced no data; clipboard left untouched"
            ));
        }
        DecodeVerdict::Failed => {
            return Err(anyhow::anyhow!("cliphist decode exited {dec_status:?}"));
        }
        DecodeVerdict::TooLarge => {
            return Err(anyhow::anyhow!(
                "cliphist decode output exceeds {max_bytes} bytes; clipboard left untouched"
            ));
        }
    }

    let mut copier = std::process::Command::new(tools.wl_copy)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn wl-copy: {e}"))?;
    if let Some(mut stdin) = copier.stdin.take() {
        // `stdin` drops at the end of this block, closing the pipe (EOF).
        if let Err(e) = stdin.write_all(&payload) {
            let _ = copier.kill();
            let _ = copier.wait();
            return Err(anyhow::anyhow!("write wl-copy stdin: {e}"));
        }
    }
    let cp_status = copier
        .wait()
        .map_err(|e| anyhow::anyhow!("wait wl-copy: {e}"))?;
    if !cp_status.success() {
        return Err(anyhow::anyhow!("wl-copy exited {cp_status:?}"));
    }
    Ok(())
}

/// Delete `id` from cliphist by piping its bare id into `cliphist
/// delete` — one subprocess, no `cliphist list` round-trip.
///
/// # Why a bare id is enough
///
/// `cliphist delete` reads stdin line-by-line and, per each line, calls
/// its internal `extractID`: cut on the first tab, parse whatever's
/// before it (the whole line, if there's no tab) as an integer. That is
/// the *same* `extractID` cliphist's `decode` subcommand uses on its
/// bare CLI-arg id — and this file already drives `decode` with a bare
/// id in [`run_decode_to_wlcopy`] (`.arg("decode").arg(id.to_string())`,
/// no tab, no preview text). So piping a bare id into `delete`'s stdin
/// isn't new coupling to an undocumented cliphist internal; it's reusing
/// a dependency this file already has on `extractID`'s shape via the
/// paste path, just for a second subcommand.
///
/// Verified against real cliphist 0.7.0 (see #742's repro transcript)
/// and against upstream's source (`sentriz/cliphist`, `cliphist.go`):
/// `extractID` and the line-scanning `delete` loop have been in place
/// since the `delete-stdin` command was added (upstream #18) and made
/// multi-line (#63) — this isn't a recent or unstable code path.
///
/// Deleting an id cliphist doesn't currently hold (stale/already-gone)
/// is a silent no-op: `BoltDB`'s `Bucket.Delete` doesn't error on a
/// missing key, confirmed empirically too. So this still exits `Ok`
/// for a stale id, same as the old list-then-delete form did by
/// treating a list-miss as success — just via cliphist's own
/// idempotency instead of a pre-check on our side.
///
/// # Residual risk
///
/// The full `<id>\t<preview>` line `cliphist list | … | cliphist
/// delete` form is the form cliphist's own docs and `etc/cliphist/README.md`
/// show, and always carries a tab. If a future cliphist ever tightens
/// `extractID` to *require* the tab (moving from "id prefix, tab
/// optional" to "must look like a full list record"), a bare-id write
/// here would start failing while that idiomatic form kept working —
/// and nothing in this workspace's CI would catch it, since cliphist
/// isn't in the devShell. That failure is loud and self-healing, not
/// silent: `cliphist delete` would exit non-zero, this function returns
/// `Err`, [`delete`]'s caller logs a `tracing::warn!`, and the phase-2
/// authoritative [`reload_into`] that always runs afterward re-reads
/// `cliphist list`, sees the entry is still there, and the optimistically
/// pruned row reappears. No permanent data loss, no silent divergence
/// between the UI and cliphist's actual state — just a delete that
/// visibly didn't take, at which point the shared `extractID` behind
/// [`run_decode_to_wlcopy`] would almost certainly be failing too.
fn run_delete_by_id(id: u64) -> anyhow::Result<()> {
    use std::io::Write as _;

    let mut delete = std::process::Command::new("cliphist")
        .arg("delete")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn cliphist delete: {e}"))?;
    {
        let mut stdin = delete
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("cliphist delete: no stdin pipe"))?;
        stdin
            .write_all(&delete_stdin_payload(id))
            .map_err(|e| anyhow::anyhow!("write cliphist delete stdin: {e}"))?;
    }
    let status = delete
        .wait()
        .map_err(|e| anyhow::anyhow!("wait cliphist delete: {e}"))?;
    if !status.success() {
        return Err(anyhow::anyhow!("cliphist delete exited {status:?}"));
    }
    Ok(())
}

/// The exact stdin payload [`run_delete_by_id`] writes to `cliphist
/// delete`: the bare id, newline-terminated — no tab, no preview text.
/// Pulled out as its own function so the wire format is unit-testable
/// without a real cliphist binary (see the `tests` module).
fn delete_stdin_payload(id: u64) -> Vec<u8> {
    format!("{id}\n").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn decode_verdict_copies_only_successful_nonempty() {
        let max = DECODE_MAX_BYTES;
        assert_eq!(decode_verdict(true, 5, max), DecodeVerdict::Copy);
        // Stale id: non-zero exit, empty stdout.
        assert_eq!(decode_verdict(false, 0, max), DecodeVerdict::Failed);
        assert_eq!(decode_verdict(false, 5, max), DecodeVerdict::Failed);
        assert_eq!(decode_verdict(true, 0, max), DecodeVerdict::Empty);
        let over = usize::try_from(DECODE_MAX_BYTES).unwrap() + 1;
        assert_eq!(decode_verdict(true, over, max), DecodeVerdict::TooLarge);
        // The overflow case as it actually arrives: killed for it (#1559).
        assert_eq!(decode_verdict(false, over, max), DecodeVerdict::TooLarge);
        let at_cap = usize::try_from(DECODE_MAX_BYTES).unwrap();
        assert_eq!(decode_verdict(true, at_cap, max), DecodeVerdict::Copy);
    }

    // ── run_decode_to_wlcopy (real child processes, stand-in scripts) ───────
    //
    // `decode_verdict` above is the decision; these run the orchestration
    // around it — the spawn, the bounded read, the kill, the reap and the
    // hand-off to `wl-copy` — against `#!/bin/sh` stand-ins for both programs,
    // in a scratch directory (#1559).

    /// Install a `#!/bin/sh` stand-in at `dir/name`, written by a `sh` child
    /// and **never by this process**.
    ///
    /// Not `std::fs::write` plus a chmod, because of `ETXTBSY`: a sibling test
    /// that forks hands its child a copy of every fd this process has open at
    /// that instant (`O_CLOEXEC` closes them only at the child's `exec`), and
    /// `execve` refuses a file anyone holds open for writing. Same fix as
    /// `hytte-sensors`' `gpu::tests::script` (#1567).
    fn script(dir: &std::path::Path, name: &str, body: &str) -> String {
        use std::io::Write as _;
        use std::process::Command;
        let path = dir.join(name);
        let mut writer = Command::new("/bin/sh")
            .args(["-c", r#"cat > "$1" && chmod 755 "$1""#, "sh"])
            .arg(&path)
            .stdin(Stdio::piped())
            .spawn()
            .expect("spawn the script writer");
        writer
            .stdin
            .take()
            .expect("piped")
            .write_all(format!("#!/bin/sh\n{body}\n").as_bytes())
            .expect("hand the script over");
        assert!(
            writer.wait().expect("reap the script writer").success(),
            "could not install {}",
            path.display()
        );
        path.to_str().expect("a UTF-8 scratch path").to_owned()
    }

    /// A scratch directory holding a `cliphist` stand-in with `decode_body` as
    /// its script and a `wl-copy` stand-in with `copy_body`.
    struct Stand {
        dir: tempfile::TempDir,
        cliphist: String,
        wl_copy: String,
    }

    impl Stand {
        fn new(decode_body: &str, copy_body: &str) -> Self {
            let dir = tempfile::tempdir().expect("a scratch dir");
            let cliphist = script(dir.path(), "cliphist", decode_body);
            let wl_copy = script(dir.path(), "wl-copy", copy_body);
            Self {
                dir,
                cliphist,
                wl_copy,
            }
        }

        /// `wl-copy` as a stand-in that keeps what it was given in `copied`.
        fn copying(decode_body: &str) -> Self {
            Self::new(decode_body, r#"cat > "$(dirname "$0")/copied""#)
        }

        fn paste(&self, id: u64, max_bytes: u64, timeout: Duration) -> anyhow::Result<()> {
            run_decode_to_wlcopy_with(
                &PasteTools {
                    cliphist: &self.cliphist,
                    wl_copy: &self.wl_copy,
                    max_bytes,
                    timeout,
                },
                id,
            )
        }

        fn path(&self, name: &str) -> std::path::PathBuf {
            self.dir.path().join(name)
        }

        /// What `wl-copy` was handed, or `None` if it never ran.
        fn copied(&self) -> Option<Vec<u8>> {
            std::fs::read(self.path("copied")).ok()
        }
    }

    const GENEROUS: Duration = Duration::from_secs(10);

    fn message(result: anyhow::Result<()>) -> String {
        result.expect_err("the paste must fail").to_string()
    }

    /// A decoded entry reaches `wl-copy` byte for byte, asked for by its id.
    #[test]
    fn a_decoded_entry_reaches_wl_copy_byte_for_byte() {
        let stand = Stand::copying(
            r#"printf '%s' "$*" > "$(dirname "$0")/args"; printf 'hello\tclip\n'"#,
        );
        stand.paste(42, 1024, GENEROUS).expect("the paste");
        assert_eq!(stand.copied().as_deref(), Some(&b"hello\tclip\n"[..]));
        assert_eq!(
            std::fs::read_to_string(stand.path("args")).expect("cliphist ran"),
            "decode 42"
        );
    }

    /// A stale id: `cliphist decode` fails with nothing on stdout. `wl-copy`
    /// never runs, so the live clipboard is not blanked (#1497).
    #[test]
    fn a_failed_decode_leaves_the_clipboard_alone() {
        let stand = Stand::copying("exit 1");
        let error = message(stand.paste(7, 1024, GENEROUS));
        assert!(error.contains("exited"), "{error}");
        assert_eq!(stand.copied(), None, "wl-copy ran for a failed decode");
    }

    /// A clean exit with nothing decoded copies nothing either.
    #[test]
    fn an_empty_decode_leaves_the_clipboard_alone() {
        let stand = Stand::copying("exit 0");
        let error = message(stand.paste(7, 1024, GENEROUS));
        assert!(error.contains("no data"), "{error}");
        assert_eq!(stand.copied(), None, "wl-copy ran for an empty decode");
    }

    /// A decoder that writes past the cap is stopped, and the paste says it
    /// was **too large**, not that the decoder "exited" (#1559). The decoder
    /// never stops writing on its own, so what ends it is ours — `SIGPIPE`
    /// once the read side closes, or the kill — and its status is always a
    /// signal.
    ///
    /// **Mutation:** check the exit status before the size in
    /// `decode_verdict` (the old order) → this reds, the message reading
    /// `cliphist decode exited ExitStatus(unix_wait_status(13))`.
    #[test]
    fn a_decode_past_the_cap_is_killed_and_reported_too_large() {
        let stand = Stand::copying("while :; do printf '%s' 0123456789abcdef; done");
        let error = message(stand.paste(7, 1024, GENEROUS));
        assert!(error.contains("exceeds 1024 bytes"), "{error}");
        assert_eq!(stand.copied(), None, "wl-copy ran for a truncated decode");
    }

    /// Exactly the cap is not past it: the whole payload is copied.
    #[test]
    fn a_decode_of_exactly_the_cap_is_copied() {
        let stand = Stand::copying("printf '%s' 0123456789abcdef");
        stand.paste(7, 16, GENEROUS).expect("the paste");
        assert_eq!(stand.copied().as_deref(), Some(&b"0123456789abcdef"[..]));
    }

    /// A decoder that stalls is killed at the timeout and reaped: the call
    /// returns long before the decoder would have, and leaves no zombie.
    #[test]
    fn a_stalled_decode_is_killed_and_reaped_at_the_timeout() {
        let stand = Stand::copying(r#"echo $$ > "$(dirname "$0")/pid"; exec sleep 30"#);
        let started = std::time::Instant::now();
        let error = message(stand.paste(7, 1024, Duration::from_millis(300)));
        assert!(error.contains("timed out"), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the timeout did not end the decode: {:?}",
            started.elapsed()
        );
        assert_eq!(stand.copied(), None, "wl-copy ran for a stalled decode");
        let pid = std::fs::read_to_string(stand.path("pid")).expect("the decoder ran");
        let proc_dir = std::path::PathBuf::from(format!("/proc/{}", pid.trim()));
        assert!(
            !proc_dir.exists(),
            "the killed decoder {} is still in /proc (a zombie)",
            pid.trim()
        );
    }

    /// A `wl-copy` that fails is reported.
    #[test]
    fn a_failing_wl_copy_is_reported() {
        let stand = Stand::new("printf 'payload'", "cat > /dev/null; exit 3");
        let error = message(stand.paste(7, 1024, GENEROUS));
        assert!(error.contains("wl-copy exited"), "{error}");
    }

    #[test]
    fn parse_list_text_only() {
        let raw = "1\thello world\n2\tanother clip\n";
        let v = parse_list(raw);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].id, 1);
        assert_eq!(v[0].kind, ClipKind::Text);
        assert_eq!(v[0].preview, "hello world");
        assert_eq!(v[1].id, 2);
        assert_eq!(v[1].preview, "another clip");
    }

    #[test]
    fn parse_list_image() {
        let raw = "42\t[[ binary data 12.3 KiB png ]]\n";
        let v = parse_list(raw);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].id, 42);
        assert_eq!(v[0].kind, ClipKind::Image);
        assert!(v[0].preview.starts_with("Image"));
    }

    #[test]
    fn parse_list_skips_garbage() {
        let raw = "not-a-line\n3\tok\nNaN\tnope\n";
        let v = parse_list(raw);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].id, 3);
        assert_eq!(v[0].preview, "ok");
    }

    #[test]
    fn parse_list_skips_empty_preview() {
        // cliphist sometimes emits "5\t\n" (id with empty preview); the
        // parser must drop such rows so the UI doesn't render blank clicks.
        let raw = "5\t\n6\treal entry\n";
        let v = parse_list(raw);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].id, 6);
    }

    #[test]
    fn parse_list_text_starting_with_single_bracket_binary_is_text() {
        // Regression: the old fallback `[binary` (single bracket) would
        // false-positive a text clip whose contents happen to start that
        // way. Real cliphist always uses `[[ binary data … ]]`.
        let raw = "9\t[binary review note]\n";
        let v = parse_list(raw);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].kind, ClipKind::Text);
        assert_eq!(v[0].preview, "[binary review note]");
    }

    #[test]
    fn parse_list_caps_at_max() {
        use std::fmt::Write as _;
        let mut raw = String::new();
        for i in 0..(MAX_ENTRIES + 10) {
            let _ = writeln!(raw, "{i}\tline {i}");
        }
        let v = parse_list(&raw);
        assert_eq!(v.len(), MAX_ENTRIES);
    }

    #[test]
    fn truncate_keeps_short() {
        assert_eq!(truncate("hi", 10), "hi");
    }

    #[test]
    fn truncate_cuts_long() {
        let long = "a".repeat(200);
        let t = truncate(&long, 10);
        assert_eq!(t.chars().count(), 11);
        assert!(t.ends_with('\u{2026}'));
    }

    // ── cliphist-delete stdin wire format (run_delete_by_id's new, smaller
    // surface after #742 dropped the cliphist-list round-trip) ─────────────

    #[test]
    fn delete_stdin_payload_is_bare_id_plus_newline() {
        // No tab, no preview text — this is the exact contract #742 leans
        // on: `cliphist delete` extracts an id prefix cutting on the first
        // tab (or the whole line, absent one), so a bare id is sufficient.
        assert_eq!(delete_stdin_payload(42), b"42\n");
    }

    #[test]
    fn delete_stdin_payload_has_no_tab() {
        // Guards against accidentally reintroducing a `<id>\t<preview>`
        // shaped payload, which is what the pre-#742 implementation sent.
        assert!(!delete_stdin_payload(7).contains(&b'\t'));
    }

    #[test]
    fn delete_stdin_payload_formats_plain_decimal() {
        // Rust's `{}` formatting for u64 is already plain decimal (no
        // grouping, no sign, no exponent), but this pins that down: a
        // non-decimal rendering would break cliphist's Go `strconv.Atoi`
        // parse on the other end.
        assert_eq!(
            delete_stdin_payload(u64::MAX),
            format!("{}\n", u64::MAX).into_bytes()
        );
    }

    // ── Optimistic post-delete reconciliation (phase 1 of `delete`) ──────────

    fn entries(ids: &[u64]) -> Vec<ClipEntry> {
        ids.iter()
            .map(|&id| ClipEntry {
                id,
                preview: format!("clip {id}"),
                kind: ClipKind::Text,
            })
            .collect()
    }

    #[test]
    fn without_id_drops_only_the_target() {
        let v = without_id(&entries(&[3, 2, 1]), 2);
        assert_eq!(v.iter().map(|e| e.id).collect::<Vec<_>>(), vec![3, 1]);
    }

    #[test]
    fn without_id_preserves_order_and_payload() {
        // The optimistic snapshot must not reshuffle surviving rows — cliphist
        // lists newest-first and the drawer renders in emit order.
        let v = without_id(&entries(&[9, 8, 7, 6]), 8);
        assert_eq!(v.iter().map(|e| e.id).collect::<Vec<_>>(), vec![9, 7, 6]);
        assert_eq!(v[0].preview, "clip 9");
        assert_eq!(v[0].kind, ClipKind::Text);
    }

    #[test]
    fn without_id_is_a_noop_for_a_missing_id() {
        let src = entries(&[3, 2, 1]);
        assert_eq!(without_id(&src, 42), src);
    }

    #[test]
    fn without_id_removes_the_head_and_the_tail() {
        assert_eq!(
            without_id(&entries(&[3, 2, 1]), 3)
                .iter()
                .map(|e| e.id)
                .collect::<Vec<_>>(),
            vec![2, 1]
        );
        assert_eq!(
            without_id(&entries(&[3, 2, 1]), 1)
                .iter()
                .map(|e| e.id)
                .collect::<Vec<_>>(),
            vec![3, 2]
        );
    }

    #[test]
    fn without_id_on_the_last_entry_yields_the_empty_state() {
        // Draining the list must produce an empty Vec, which is what drives
        // `reactive_list`'s empty placeholder ("No clipboard history").
        assert!(without_id(&entries(&[7]), 7).is_empty());
    }

    /// Regression for the delete/refresh race: the optimistic prune is the
    /// only reason an open drawer reflects a delete without being reopened.
    /// `delete()` used to fire a concurrent `refresh()` instead, whose single
    /// `cliphist list` beat the delete task's `list`+`delete` pair 20/20 —
    /// so the emitted snapshot still contained the doomed entry.
    ///
    /// This asserts the shape that fixed it: pruning is what makes the
    /// post-delete snapshot differ from the pre-delete one.
    #[test]
    fn optimistic_prune_differs_from_the_pre_delete_snapshot() {
        let before = entries(&[3, 2, 1]);
        let after = without_id(&before, 2);
        assert_ne!(
            before, after,
            "a pruned snapshot must differ from the pre-delete list, or the \
             PartialEq dedup in reload_into/refresh suppresses the emit and \
             the deleted row stays on screen"
        );
        assert!(!after.iter().any(|e| e.id == 2));
    }
}
