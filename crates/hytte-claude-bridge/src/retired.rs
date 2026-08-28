//! The retired-session map's on-disk form (#855).
//!
//! # Why this file exists at all
//!
//! [`crate::session::Titles`] holds two maps. `by_prefix` is a cache — losing
//! it costs a fresh session, which is the documented degraded-never-wrong
//! outcome. `retired` is not: it records *"this conversation's session
//! overflowed; it continues as this other title"*, and it has to **win** on
//! lookup precisely because resuming the retired session would overflow again
//! on every turn.
//!
//! Held only in memory, that fact died with the process. A bridge restart sent
//! an already-rotated conversation straight back to generation 0 — the session
//! that had overflowed — so the next turn 413'd, rotated a second time, and
//! self-healed one **visibly failed turn** later (on glass: a canned fallback
//! line instead of an answer). The bridge is a user unit, so that happened on
//! every `systemctl --user restart`, every deploy, every reboot.
//!
//! #704 is what made it worth fixing. Under transcript-derived identity a
//! conversation's key moved whenever its prefix moved, so sessions were
//! short-lived and rotation was a ~10³-turn formality most conversations never
//! reached. An explicit identity is stable *by design*, so a plugin's session
//! is permanent, grows monotonically, and rotation becomes routine.
//!
//! # What is persisted, and what deliberately is not
//!
//! **`retired` only.** `by_prefix` stays in memory, for three reasons:
//!
//! - it is a cache whose miss is *correct* behaviour (a fresh session with a
//!   cold prompt cache), not a wrong answer;
//! - it is written **twice per answered turn**, so persisting it would put a
//!   disk write on the hot path to buy back a cache — where `retired` is
//!   written about once per context window;
//! - after #704 an identity-carrying caller does not write it at all, and those
//!   are exactly the callers rotation now matters for.
//!
//! # The format
//!
//! Boring, greppable JSON, whole-map rewrite (`{"version":1,"retired":[…]}`).
//! Each entry states its identity **space** — the [`crate::session::Key`]
//! variant — alongside the digest as the same 16 lowercase hex characters the
//! title itself renders, so a human can line an entry up against the title it
//! points at. The two spaces must never be confused on the way back in: `Key`
//! keeps them disjoint by construction in memory, and this file keeps them
//! disjoint on disk rather than collapsing both to a bare integer.
//!
//! Entries are written **oldest first**, the order `session::Bounded` evicts
//! in, so a file read back into a smaller map drops exactly what the live map
//! would have dropped.
//!
//! # Corruption is never fatal
//!
//! Every failure — missing, empty, truncated, wrong shape, unknown version,
//! unreadable entry, absurdly large, unreadable for any other reason —
//! degrades to **an empty map**, which is precisely the behaviour that shipped
//! before this file existed. A daemon that refuses to boot over a cache file is
//! a worse bug than the one being fixed here, so [`load`] returns a `Vec` and
//! never a `Result`. A bad file is also not left to rot: the next rotation
//! rewrites it whole.
//!
//! One file-wide `Vec` is returned rather than salvaging the good entries out
//! of a bad file, deliberately. This file has exactly one writer, which writes
//! it atomically; an entry that does not parse means something *else* edited
//! it, and half-trusting an edited file is a worse guess than distrusting it.

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::session::{KEY_HEX_LEN, Key, TITLE_PREFIX};

/// The file's name inside `CLAUDE_BRIDGE_STATE_DIR`.
pub const FILE: &str = "retired-sessions.json";

/// The only format version this build writes, and the only one it reads.
///
/// A file from a future (or unrecognised) version is *ignored*, not guessed at
/// — same degradation as any other unreadable file.
const VERSION: u32 = 1;

/// Largest file [`load`] will read, as a guard against parsing something
/// pathological into memory at startup.
///
/// The real map is bounded at a few hundred entries of ~90 bytes, so a healthy
/// file is tens of kilobytes; a megabyte is already two orders of magnitude of
/// headroom. Enforced while reading rather than after, so an enormous file
/// costs one bounded read instead of a `serde_json` allocation storm.
const MAX_BYTES: u64 = 1 << 20;

/// Distinguishes the temp files of two writes that overlap in time.
///
/// Saves are serialised in practice (every one happens under the bridge's
/// `titles` mutex), but `Titles` is a plain type that nothing stops a second
/// caller from holding, and combined with the pid this makes the temp name
/// unique across the machine regardless.
static TMP_TICKET: AtomicU64 = AtomicU64::new(0);

/// The whole file.
#[derive(Serialize, Deserialize)]
struct Document {
    version: u32,
    retired: Vec<Entry>,
}

/// One retirement: "this conversation continues under this title".
#[derive(Serialize, Deserialize)]
struct Entry {
    space: Space,
    key: String,
    title: String,
}

/// Which identity space an entry's digest was computed in — the on-disk
/// spelling of [`Key`]'s variants.
///
/// Written out rather than derived from `Key` directly so the file format is
/// stated here instead of being an accident of the enum's Rust names, and so a
/// variant rename is a compile error rather than a silently invalidated file.
#[derive(Serialize, Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum Space {
    /// [`Key::Transcript`] — the digest of a conversation's transcript prefix.
    Transcript,
    /// [`Key::User`] — the digest of a caller-supplied identity (#704).
    User,
}

impl Entry {
    /// Render one live map entry.
    fn new(key: Key, title: &str) -> Self {
        let (space, digest) = match key {
            Key::Transcript(digest) => (Space::Transcript, digest),
            Key::User(digest) => (Space::User, digest),
        };
        Self {
            space,
            key: format!("{digest:016x}"),
            title: title.to_owned(),
        }
    }

    /// Read one entry back, or `None` if it is not something this bridge could
    /// have written.
    ///
    /// Strict on both halves, in the same spirit as
    /// `session::split_generation`:
    ///
    /// - the digest must be exactly [`KEY_HEX_LEN`] hex characters, so a
    ///   truncated or hand-typed one is rejected rather than zero-extended into
    ///   some *other* conversation's key;
    /// - the title must carry [`TITLE_PREFIX`], so no edit of this file can
    ///   point the bridge at a session it did not mint — a human's own
    ///   `claude` sessions live in the same store, under titles the prefix
    ///   exists to keep separate.
    fn key(&self) -> Option<Key> {
        if self.key.len() != KEY_HEX_LEN || !self.key.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        if !self.title.starts_with(TITLE_PREFIX) {
            return None;
        }
        let digest = u64::from_str_radix(&self.key, 16).ok()?;
        Some(match self.space {
            Space::Transcript => Key::Transcript(digest),
            Space::User => Key::User(digest),
        })
    }
}

/// Read the retired map back, oldest entry first, capped at `cap`.
///
/// **Never fails.** Anything unreadable is logged and yields an empty map — see
/// the module docs for why that is the whole point.
///
/// `cap` is applied here as well as by the map that consumes this, so a file
/// written by a build with a larger capacity (or edited by hand) cannot hand
/// back more than the live map would hold. The **last** `cap` entries survive,
/// matching the oldest-first eviction the file is written in.
pub fn load(path: &Path, cap: usize) -> Vec<(Key, String)> {
    let Some(bytes) = read_bounded(path) else {
        return Vec::new();
    };
    let doc: Document = match serde_json::from_slice(&bytes) {
        Ok(doc) => doc,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "the retired-session map is unreadable; starting with none, and the next rotation \
                 will rewrite it",
            );
            return Vec::new();
        }
    };
    if doc.version != VERSION {
        tracing::warn!(
            path = %path.display(),
            found = doc.version,
            expected = VERSION,
            "the retired-session map is a format this build does not read; starting with none",
        );
        return Vec::new();
    }
    let mut entries = Vec::with_capacity(doc.retired.len());
    for entry in &doc.retired {
        let Some(key) = entry.key() else {
            tracing::warn!(
                path = %path.display(),
                title = %entry.title,
                "the retired-session map holds an entry this bridge could not have written; \
                 starting with none",
            );
            return Vec::new();
        };
        entries.push((key, entry.title.clone()));
    }
    let cap = cap.max(1);
    if entries.len() > cap {
        tracing::warn!(
            path = %path.display(),
            found = entries.len(),
            cap,
            "the retired-session map holds more entries than this build maps; keeping the newest",
        );
        entries.drain(..entries.len() - cap);
    }
    if !entries.is_empty() {
        tracing::info!(
            path = %path.display(),
            entries = entries.len(),
            "restored the retired-session map, so a rotated conversation resumes where it left off",
        );
    }
    entries
}

/// The file's bytes, or `None` (logged) if it is missing, too big, or
/// unreadable.
fn read_bounded(path: &Path) -> Option<Vec<u8>> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // The first-boot case, and the case right after the state dir is
            // wiped. Neither is a problem worth a warning.
            tracing::debug!(
                path = %path.display(),
                "no retired-session map on disk yet; starting with none",
            );
            return None;
        }
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "could not open the retired-session map; starting with none",
            );
            return None;
        }
    };
    let mut bytes = Vec::new();
    if let Err(e) = file.take(MAX_BYTES + 1).read_to_end(&mut bytes) {
        tracing::warn!(
            path = %path.display(),
            error = %e,
            "could not read the retired-session map; starting with none",
        );
        return None;
    }
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_BYTES {
        tracing::warn!(
            path = %path.display(),
            max_bytes = MAX_BYTES,
            "the retired-session map is implausibly large; refusing to parse it",
        );
        return None;
    }
    Some(bytes)
}

/// Replace the whole file with `entries`, oldest first, creating the parent
/// directory.
///
/// **Atomic**: the body lands in a temp file beside the target, is `fsync`ed,
/// and is then `rename(2)`d over it, so a crash mid-write leaves the previous
/// map whole rather than a truncated one that would then be rejected on the
/// next boot. Hand-rolled rather than reused from `hytte_config::file`: this
/// crate deliberately links **nothing** in the hytte crate graph, and pulling
/// a library in for twenty lines would trade that for no benefit — a
/// daemon-private cache in the daemon's own state dir needs none of the
/// symlink-following or permission-preserving that helper carries for
/// hand-edited configs.
///
/// The parent directory is **not** `fsync`ed. Losing the rename to a power cut
/// leaves the whole previous file in place, which is a state every reader here
/// already handles — the same call `hytte_config::file::Durability::FileOnly`
/// declines, for the same reason.
///
/// A whole-map rewrite rather than an append log because the write happens
/// about once per context window: the entire point of `retired` is that it is
/// not churned by traffic.
pub fn save(path: &Path, entries: &[(Key, &str)]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let doc = Document {
        version: VERSION,
        retired: entries
            .iter()
            .map(|(key, title)| Entry::new(*key, title))
            .collect(),
    };
    let body = serde_json::to_vec_pretty(&doc).map_err(std::io::Error::other)?;
    let tmp = tmp_path(path);
    let swap = || -> std::io::Result<()> {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&body)?;
        // Without this the rename can be durable while the data is not, which
        // on a delayed-allocation filesystem resurrects exactly the truncated
        // file this is here to prevent.
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    };
    let outcome = swap();
    if outcome.is_err() {
        // Don't leave litter beside the map.
        let _ = std::fs::remove_file(&tmp);
    }
    outcome
}

/// A temp file beside `path`, on the target's own filesystem — `rename(2)` is
/// only atomic within one.
fn tmp_path(path: &Path) -> PathBuf {
    let ticket = TMP_TICKET.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .map_or_else(|| FILE.to_owned(), |n| n.to_string_lossy().into_owned());
    path.with_file_name(format!(".{name}.{}.{ticket}.tmp", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::{FILE, Key, MAX_BYTES, VERSION, load, save};
    use std::path::PathBuf;

    const CAP: usize = 64;

    /// A title this bridge could have minted for `key`, at generation 1.
    fn title(key: Key) -> String {
        format!("{}-g1", crate::session::title_for(key))
    }

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().expect("a tempdir")
    }

    fn path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join(FILE)
    }

    /// Load a file whose exact bytes the test wrote.
    fn load_body(dir: &tempfile::TempDir, body: &str) -> Vec<(Key, String)> {
        let path = path(dir);
        std::fs::write(&path, body).expect("a writable tempdir");
        load(&path, CAP)
    }

    /// The point of the whole file: what went in comes back out, in both
    /// identity spaces, still told apart.
    ///
    /// Two entries whose **digests are identical** and whose spaces differ, so
    /// this fails if the space is dropped on the way through the file (both
    /// entries would collapse onto one key) or if it is written but read back
    /// as the wrong variant.
    #[test]
    fn both_identity_spaces_round_trip_and_stay_disjoint() {
        let dir = dir();
        let transcript = Key::Transcript(0x0123_4567_89ab_cdef);
        let user = Key::User(0x0123_4567_89ab_cdef);
        let (t_title, u_title) = (title(transcript), title(user));
        save(
            &path(&dir),
            &[(transcript, t_title.as_str()), (user, u_title.as_str())],
        )
        .expect("a writable tempdir");

        let loaded = load(&path(&dir), CAP);
        assert_eq!(
            loaded,
            vec![(transcript, t_title), (user, u_title)],
            "the two spaces must survive the round trip as two distinct keys",
        );
    }

    /// The digest is written as the same 16 hex characters the title carries,
    /// so a human can line an entry up against the session it points at.
    #[test]
    fn the_digest_is_stored_as_the_hex_the_title_renders() {
        let dir = dir();
        let key = Key::User(0x0123_4567_89ab_cdef);
        save(&path(&dir), &[(key, title(key).as_str())]).expect("a writable tempdir");

        let body = std::fs::read_to_string(path(&dir)).expect("a readable file");
        assert!(body.contains("0123456789abcdef"), "{body}");
        assert!(body.contains("\"user\""), "{body}");
        assert!(
            body.contains("hytte-bridge-u-0123456789abcdef-g1"),
            "{body}"
        );
    }

    /// A missing file is the first-boot case, not an error.
    #[test]
    fn a_missing_file_loads_empty() {
        let dir = dir();
        assert!(load(&path(&dir), CAP).is_empty());
    }

    #[test]
    fn an_empty_file_loads_empty() {
        let dir = dir();
        assert!(load_body(&dir, "").is_empty());
    }

    /// The shape a crash mid-write would leave without the atomic rename.
    #[test]
    fn truncated_json_loads_empty() {
        let dir = dir();
        let truncated = r#"{"version":1,"retired":[{"space":"user","key":"0123456789ab"#;
        assert!(load_body(&dir, truncated).is_empty());
    }

    /// Valid JSON that simply is not this document.
    #[test]
    fn json_of_the_wrong_shape_loads_empty() {
        let dir = dir();
        assert!(load_body(&dir, "[1, 2, 3]").is_empty());
        assert!(load_body(&dir, r#"{"retired": "not a list"}"#).is_empty());
        assert!(load_body(&dir, r#"{"version": 1}"#).is_empty());
        assert!(load_body(&dir, "null").is_empty());
    }

    /// A file this build does not know how to read is ignored, not guessed at.
    #[test]
    fn an_unknown_version_loads_empty() {
        let dir = dir();
        let future = format!(r#"{{"version":{},"retired":[]}}"#, VERSION + 1);
        assert!(load_body(&dir, &future).is_empty());
    }

    /// A digest that is not exactly 16 hex characters is rejected rather than
    /// zero-extended into some other conversation's key.
    #[test]
    fn a_malformed_digest_loads_empty() {
        let dir = dir();
        for key in [
            "",
            "0123456789abcde",
            "0123456789abcdef0",
            "zzzzzzzzzzzzzzzz",
        ] {
            let body = format!(
                r#"{{"version":1,"retired":[{{"space":"user","key":"{key}","title":"hytte-bridge-u-0123456789abcdef-g1"}}]}}"#
            );
            assert!(load_body(&dir, &body).is_empty(), "accepted key {key:?}");
        }
    }

    /// No edit of this file can point the bridge at a session it did not mint —
    /// the human's own `claude` sessions live in the same store.
    #[test]
    fn a_title_this_bridge_never_minted_loads_empty() {
        let dir = dir();
        let body = r#"{"version":1,"retired":[{"space":"user","key":"0123456789abcdef","title":"my-own-important-session"}]}"#;
        assert!(load_body(&dir, body).is_empty());
    }

    /// An unrecognised identity space is not a third space, it is a bad file.
    #[test]
    fn an_unknown_space_loads_empty() {
        let dir = dir();
        let body = r#"{"version":1,"retired":[{"space":"guess","key":"0123456789abcdef","title":"hytte-bridge-0123456789abcdef-g1"}]}"#;
        assert!(load_body(&dir, body).is_empty());
    }

    /// Something pathological must cost one bounded read, not the daemon's
    /// startup.
    #[test]
    fn an_implausibly_large_file_loads_empty() {
        let dir = dir();
        let key = Key::User(1);
        save(&path(&dir), &[(key, title(key).as_str())]).expect("a writable tempdir");
        // Valid JSON, just absurd: pad the document with whitespace past the
        // ceiling, so this can only be rejected by the size guard.
        let mut body = std::fs::read_to_string(path(&dir)).expect("a readable file");
        let pad = usize::try_from(MAX_BYTES).expect("a 64-bit test host") + 1;
        body.push_str(&" ".repeat(pad));
        assert!(load_body(&dir, &body).is_empty());
    }

    /// A file from a build that mapped more conversations must not let this one
    /// exceed its own cap — and the entries it keeps are the newest, matching
    /// the oldest-first order the file is written in.
    #[test]
    fn an_over_cap_file_keeps_the_newest_up_to_the_cap() {
        let dir = dir();
        let keys: Vec<Key> = (0..10).map(Key::User).collect();
        let titles: Vec<String> = keys.iter().map(|k| title(*k)).collect();
        let entries: Vec<(Key, &str)> = keys
            .iter()
            .zip(&titles)
            .map(|(k, t)| (*k, t.as_str()))
            .collect();
        save(&path(&dir), &entries).expect("a writable tempdir");

        let loaded = load(&path(&dir), 3);
        assert_eq!(loaded.len(), 3);
        assert_eq!(
            loaded.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            vec![Key::User(7), Key::User(8), Key::User(9)],
        );
    }

    /// The write replaces the file whole: yesterday's entries do not linger.
    #[test]
    fn a_save_replaces_the_previous_map_rather_than_appending() {
        let dir = dir();
        let (old, new) = (Key::User(1), Key::User(2));
        save(&path(&dir), &[(old, title(old).as_str())]).expect("a writable tempdir");
        save(&path(&dir), &[(new, title(new).as_str())]).expect("a writable tempdir");

        let loaded = load(&path(&dir), CAP);
        assert_eq!(loaded, vec![(new, title(new))]);
    }

    /// The state dir need not exist yet — in `api` mode nothing else creates
    /// it, and a rotation must still be able to record itself.
    #[test]
    fn a_save_creates_the_state_dir() {
        let dir = dir();
        let nested = dir.path().join("state").join("hytte-claude-bridge");
        let path = nested.join(FILE);
        let key = Key::User(3);
        save(&path, &[(key, title(key).as_str())]).expect("a creatable state dir");
        assert_eq!(load(&path, CAP), vec![(key, title(key))]);
    }

    /// A completed write leaves the directory holding exactly the map — the
    /// temp file is renamed away, not left beside it.
    #[test]
    fn a_write_leaves_no_temp_file_behind() {
        let dir = dir();
        let key = Key::User(4);
        save(&path(&dir), &[(key, title(key).as_str())]).expect("a writable tempdir");
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .expect("a readable tempdir")
            .filter_map(|e| Some(e.ok()?.file_name().to_string_lossy().into_owned()))
            .collect();
        assert_eq!(names, vec![FILE.to_owned()], "{names:?}");
    }

    /// An unwritable location is a `warn`-worthy `Err`, never a panic: the
    /// caller degrades to "this rotation is not durable", which is the
    /// behaviour that shipped before this file existed.
    #[test]
    fn an_unwritable_target_is_an_error_not_a_panic() {
        let dir = dir();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "not a directory").expect("a writable tempdir");
        let key = Key::User(5);
        assert!(save(&blocker.join(FILE), &[(key, title(key).as_str())]).is_err());
    }

    /// An empty map is a legal, readable file — not something that has to be
    /// deleted to be understood.
    #[test]
    fn an_empty_map_round_trips() {
        let dir = dir();
        save(&path(&dir), &[]).expect("a writable tempdir");
        assert!(load(&path(&dir), CAP).is_empty());
        assert!(path(&dir).exists());
    }
}
