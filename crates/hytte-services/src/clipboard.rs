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
//! * [`paste_entry(id)`] — fire-and-forget. Pipes `cliphist decode <id>`
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
//! Delete by id is supported via [`delete`]. Implementation runs
//! `cliphist list` to get the exact line cliphist recognises, then
//! pipes that line into `cliphist delete`. Two subprocess calls per
//! delete; acceptable since deletion is user-initiated.
//!
//! No clip pinning, no search/filter UI, no multi-select, no rich-format
//! paste. The page is a plain history list with click-to-paste.

use futures_signals::signal::{Mutable, Signal};
use hytte_reactive::{registry, runtime, Service};
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

    runtime::handle().spawn_blocking(move || {
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
    });
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

/// Delete a history entry by id. Re-runs `cliphist list` to obtain the
/// exact line cliphist will recognize, then pipes that line into
/// `cliphist delete`. Refreshes [`history()`] afterwards.
///
/// Fire-and-forget; failures are logged at warn.
pub fn delete(id: u64) {
    runtime::handle().spawn_blocking(move || {
        if let Err(e) = run_delete_by_id(id) {
            tracing::warn!(id, error = %e, "clipboard: delete failed");
        }
    });
    refresh();
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
    let inside = preview
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim();
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

/// Run `cliphist decode <id> | wl-copy`. Plumbs cliphist's stdout into
/// wl-copy's stdin via the standard library's `Stdio` pipe. Both children
/// are spawned and waited on; if either fails the error surfaces.
fn run_decode_to_wlcopy(id: u64) -> anyhow::Result<()> {
    let mut decoder = std::process::Command::new("cliphist")
        .arg("decode")
        .arg(id.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn cliphist decode: {e}"))?;

    let decoder_stdout = decoder
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("cliphist decode: no stdout pipe"))?;

    let mut copier = std::process::Command::new("wl-copy")
        .stdin(Stdio::from(decoder_stdout))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn wl-copy: {e}"))?;

    let dec_status = decoder
        .wait()
        .map_err(|e| anyhow::anyhow!("wait cliphist decode: {e}"))?;
    let cp_status = copier
        .wait()
        .map_err(|e| anyhow::anyhow!("wait wl-copy: {e}"))?;

    if !dec_status.success() {
        return Err(anyhow::anyhow!("cliphist decode exited {dec_status:?}"));
    }
    if !cp_status.success() {
        return Err(anyhow::anyhow!("wl-copy exited {cp_status:?}"));
    }
    Ok(())
}

/// Find the `<id>\t<preview>` line in `cliphist list` output whose
/// integer prefix equals `id`. Returns the line trimmed of the trailing
/// newline (suitable for piping into `cliphist delete` with an explicit
/// `\n` appended).
fn select_delete_line(list_output: &str, id: u64) -> Option<String> {
    for line in list_output.lines() {
        let Some((id_part, _)) = line.split_once('\t') else {
            continue;
        };
        let Ok(parsed) = id_part.trim().parse::<u64>() else {
            continue;
        };
        if parsed == id {
            return Some(line.to_string());
        }
    }
    None
}

fn run_delete_by_id(id: u64) -> anyhow::Result<()> {
    use std::io::Write as _;

    let list = std::process::Command::new("cliphist")
        .arg("list")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| anyhow::anyhow!("spawn cliphist list (for delete): {e}"))?;
    if !list.status.success() {
        return Err(anyhow::anyhow!("cliphist list (for delete) exited {:?}", list.status));
    }
    let stdout = String::from_utf8_lossy(&list.stdout);
    let Some(line) = select_delete_line(&stdout, id) else {
        // Entry already gone (concurrent delete, or id stale). Treat as
        // success so the caller's refresh still runs.
        return Ok(());
    };

    let mut delete = std::process::Command::new("cliphist")
        .arg("delete")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn cliphist delete: {e}"))?;
    {
        let mut stdin = delete.stdin.take()
            .ok_or_else(|| anyhow::anyhow!("cliphist delete: no stdin pipe"))?;
        stdin.write_all(line.as_bytes())
            .map_err(|e| anyhow::anyhow!("write cliphist delete stdin: {e}"))?;
        stdin.write_all(b"\n")
            .map_err(|e| anyhow::anyhow!("write cliphist delete newline: {e}"))?;
    }
    let status = delete.wait()
        .map_err(|e| anyhow::anyhow!("wait cliphist delete: {e}"))?;
    if !status.success() {
        return Err(anyhow::anyhow!("cliphist delete exited {status:?}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(t.chars().count() == 11);
        assert!(t.ends_with('\u{2026}'));
    }

    #[test]
    fn select_delete_line_finds_matching_id() {
        let raw = "1\thello\n42\ttarget\n3\tnope\n";
        assert_eq!(select_delete_line(raw, 42), Some("42\ttarget".to_string()));
    }

    #[test]
    fn select_delete_line_returns_none_when_id_missing() {
        let raw = "1\thello\n3\tnope\n";
        assert_eq!(select_delete_line(raw, 42), None);
    }

    #[test]
    fn select_delete_line_skips_garbage_rows() {
        let raw = "garbage-line\n42\ttarget\n";
        assert_eq!(select_delete_line(raw, 42), Some("42\ttarget".to_string()));
    }
}
