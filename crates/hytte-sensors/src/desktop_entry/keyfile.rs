//! The slice of `GLib`'s `GKeyFile` that `g_desktop_app_info_load_file` leans
//! on, ported from the source of the `GLib` the shell links (2.88.3,
//! `glib/gkeyfile.c`).
//!
//! It is a port rather than "a trivial INI subset" for one reason: **GIO
//! drops a whole desktop entry when its key file does not parse.** A line
//! that is not a comment, a group or a `key=value` pair, an invalid key
//! name, a key before the first group, an `Encoding=` other than UTF-8 —
//! each makes `g_key_file_load_from_file` fail, and the entry is then absent
//! from `g_app_info_get_all()` although its file still masks a same-id file
//! in a lower-precedence directory. A lenient parser would resolve names the
//! native page cannot see. What is ported, rule by rule:
//!
//! - **Lines** (`g_key_file_parse_data`): split on `\n`, one `\r` before it
//!   dropped; leading ASCII whitespace (`GLib`'s `g_ascii_isspace`, which
//!   is Rust's `is_ascii_whitespace`: `\v` is **not** in it) is skipped;
//!   empty or `#` lines are comments.
//! - **Groups** (`g_key_file_line_is_group`, `g_key_file_is_group_name`):
//!   `[name]` with only spaces or tabs after the `]`; the name non-empty and
//!   free of `[`, `]` and ASCII control characters. A repeated group merges
//!   into the first one of that name; the **start group** is the first group
//!   the file names.
//! - **Pairs** (`g_key_file_parse_key_value_pair`, `g_key_file_is_key_name`):
//!   split at the first `=` (a line starting with `=` is an error); the key
//!   loses trailing whitespace and must be valid — non-empty, no leading or
//!   trailing space, no `[`/`]` except one trailing `[locale]` of
//!   alphanumerics and `-_.@`; the value loses leading whitespace and keeps
//!   the rest up to the first NUL byte (`g_strndup`). A later duplicate key
//!   replaces an earlier one.
//! - **`Encoding=`** in the start group must read `UTF-8` (any case).
//! - **Strings** (`g_key_file_get_string`, `…_parse_value_as_string`): the
//!   raw value must be UTF-8, and the escapes `\s \n \t \r \\` are decoded;
//!   any other escape, or a `\` at the end, makes the value invalid.
//! - **Localised strings** (`g_key_file_get_locale_string`): the first
//!   `key[lang]` that reads as a valid string, for each language name in
//!   order, else the plain `key`.
//! - **Booleans** (`…_parse_value_as_boolean`): `true` or `1` (trailing
//!   whitespace ignored) is true, anything else false.
//!
//! Not ported, because nothing here reads it: comments are dropped rather
//! than kept, string lists are not parsed, and translations for languages
//! nobody asked for are kept (`GLib` discards them at load; the lookup is
//! exact either way, so the answer is the same).

use std::collections::HashMap;

/// The group every desktop-entry key lives in.
pub(super) const DESKTOP_ENTRY: &str = "Desktop Entry";

/// A value that is present but cannot be read as a string: not UTF-8, or an
/// escape `GLib` refuses. GIO drops an entry whose `Path=`, `TryExec=` or
/// `Exec=` is in this state (`is_invalid_key_error`), which is why it is
/// distinct from an absent key.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Invalid;

/// A parsed key file: its start group's name, and the `[Desktop Entry]`
/// group's pairs (the only group anything reads — the other groups are
/// parsed so a malformed line in them still fails the file, then dropped).
#[derive(Debug, Default)]
pub(super) struct KeyFile {
    start_group: Option<String>,
    entry: HashMap<String, Vec<u8>>,
}

impl KeyFile {
    /// Parse a whole key file; `None` wherever `g_key_file_load_from_file`
    /// reports an error.
    pub(super) fn parse(data: &[u8]) -> Option<Self> {
        let mut file = Self::default();
        // The group the next pair lands in; `None` before the first group.
        let mut current: Option<String> = None;
        let mut segments = data.split(|&b| b == b'\n').peekable();
        while let Some(segment) = segments.next() {
            // GLib drops a `\r` only when a `\n` follows it, i.e. on every
            // segment but the last.
            let line = if segments.peek().is_some() {
                segment.strip_suffix(b"\r").unwrap_or(segment)
            } else {
                segment
            };
            file.parse_line(line, &mut current)?;
        }
        Some(file)
    }

    /// One line — `g_key_file_parse_line`.
    fn parse_line(&mut self, line: &[u8], current: &mut Option<String>) -> Option<()> {
        let start = line
            .iter()
            .position(|&b| !is_space(b))
            .unwrap_or(line.len());
        let line = &line[start..];
        if line.is_empty() || line[0] == b'#' {
            return Some(());
        }
        if is_group_line(line) {
            let close = line.iter().rposition(|&b| b == b']')?;
            let name = &line[1..close];
            if name.is_empty()
                || name
                    .iter()
                    .any(|&b| b == b'[' || b == b']' || b.is_ascii_control())
            {
                return None;
            }
            let name = String::from_utf8_lossy(name).into_owned();
            if self.start_group.is_none() {
                self.start_group = Some(name.clone());
            }
            *current = Some(name);
            return Some(());
        }
        let eq = line.iter().position(|&b| b == b'=')?;
        if eq == 0 {
            // `g_key_file_line_is_key_value_pair`: a key must be non-empty.
            return None;
        }
        // Before the first group: "Key file does not start with a group".
        let group = current.as_deref()?;
        let key_end = line[..eq]
            .iter()
            .rposition(|&b| !is_space(b))
            .map_or(0, |i| i + 1);
        let key = &line[..key_end];
        if !is_key_name(key) {
            return None;
        }
        let value = &line[eq + 1..];
        let value = &value[value
            .iter()
            .position(|&b| !is_space(b))
            .unwrap_or(value.len())..];
        if self.start_group.as_deref() == Some(group)
            && key == b"Encoding"
            && !value.eq_ignore_ascii_case(b"UTF-8")
        {
            return None;
        }
        if group == DESKTOP_ENTRY {
            // `pair->value = g_strndup (value_start, value_len)`
            // (`glib/gkeyfile.c:1469`): the copy stops at a NUL byte. The
            // `Encoding=` check above compares the full length, as `GLib`'s
            // does, so it keeps the uncut value.
            let value = value.split(|&b| b == 0).next().unwrap_or(value);
            self.entry
                .insert(String::from_utf8_lossy(key).into_owned(), value.to_vec());
        }
        Some(())
    }

    /// The first group the file names — `g_key_file_get_start_group`.
    pub(super) fn start_group(&self) -> Option<&str> {
        self.start_group.as_deref()
    }

    /// `key` in `[Desktop Entry]` as a string — `g_key_file_get_string`.
    /// `Ok(None)` when the key is absent, [`Invalid`] when it is present but
    /// unreadable.
    pub(super) fn string(&self, key: &str) -> Result<Option<String>, Invalid> {
        let Some(raw) = self.entry.get(key) else {
            return Ok(None);
        };
        let raw = std::str::from_utf8(raw).map_err(|_| Invalid)?;
        unescape(raw).map(Some)
    }

    /// `key` in `[Desktop Entry]`, translated — `g_key_file_get_locale_string`
    /// with the process's language names (`languages`, most preferred first,
    /// already cut at `GLib`'s `C`).
    pub(super) fn locale_string(&self, key: &str, languages: &[String]) -> Option<String> {
        languages
            .iter()
            .find_map(|lang| self.string(&format!("{key}[{lang}]")).ok().flatten())
            .or_else(|| self.string(key).ok().flatten())
    }

    /// `key` in `[Desktop Entry]` as a boolean — `g_key_file_get_boolean`,
    /// whose error case reads as `false` at every call site GIO has here.
    pub(super) fn boolean(&self, key: &str) -> bool {
        self.entry.get(key).is_some_and(|raw| {
            let end = raw.iter().rposition(|&b| !is_space(b)).map_or(0, |i| i + 1);
            matches!(&raw[..end], b"true" | b"1")
        })
    }
}

/// `g_ascii_isspace`: space, `\t`, `\n`, `\f`, `\r` — the bytes whose entry
/// in `GLib`'s `ascii_table_data` carries `G_ASCII_SPACE`
/// (`glib/gstrfuncs.c:264`, the row for `0x08`–`0x0f`). Not `\v`: its entry is `0x004`, a control
/// character only. That is exactly Rust's `is_ascii_whitespace`.
fn is_space(b: u8) -> bool {
    b.is_ascii_whitespace()
}

/// `g_key_file_line_is_group`: `[`, then a `]`, then only spaces or tabs.
fn is_group_line(line: &[u8]) -> bool {
    if line.first() != Some(&b'[') {
        return false;
    }
    let Some(close) = line.iter().position(|&b| b == b']') else {
        return false;
    };
    line[close + 1..].iter().all(|&b| b == b' ' || b == b'\t')
}

/// `g_key_file_is_key_name`.
fn is_key_name(key: &[u8]) -> bool {
    let base = key
        .iter()
        .position(|&b| b == b'[' || b == b']' || b == 0)
        .unwrap_or(key.len());
    if base == 0 || key[0] == b' ' || key[base - 1] == b' ' {
        return false;
    }
    let rest = &key[base..];
    if rest.is_empty() {
        return true;
    }
    // Only one shape may follow the base name: `[locale]`, and nothing after.
    let Some(locale) = rest.strip_prefix(b"[").and_then(|r| r.strip_suffix(b"]")) else {
        return false;
    };
    let Ok(locale) = std::str::from_utf8(locale) else {
        return false;
    };
    locale
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '@'))
}

/// `g_key_file_parse_value_as_string`'s escape decoding.
fn unescape(raw: &str) -> Result<String, Invalid> {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        out.push(match chars.next() {
            Some('s') => ' ',
            Some('n') => '\n',
            Some('t') => '\t',
            Some('r') => '\r',
            Some('\\') => '\\',
            _ => return Err(Invalid),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{DESKTOP_ENTRY, Invalid, KeyFile};

    fn parse(text: &str) -> Option<KeyFile> {
        KeyFile::parse(text.as_bytes())
    }

    /// The ordinary file: comments, blank lines, whitespace around `=`, a
    /// CRLF line ending, and a second group whose keys never reach the
    /// `[Desktop Entry]` lookup.
    #[test]
    fn an_ordinary_entry_parses() {
        let file = parse(
            "# a comment\n\n[Desktop Entry]\nType=Application\n  Name = Firefox  \r\n\
             Exec=firefox %U\n\n[Desktop Action new-window]\nName=New Window\n",
        )
        .expect("parses");
        assert_eq!(file.start_group(), Some(DESKTOP_ENTRY));
        assert_eq!(file.string("Type"), Ok(Some("Application".to_owned())));
        assert_eq!(
            file.string("Name"),
            Ok(Some("Firefox  ".to_owned())),
            "leading whitespace goes, trailing stays, the CR before LF goes",
        );
        assert_eq!(file.string("Exec"), Ok(Some("firefox %U".to_owned())));
        assert_eq!(file.string("Icon"), Ok(None));
    }

    /// **Another group's keys never answer a `[Desktop Entry]` lookup** —
    /// even when that group comes first in a later block, and even when the
    /// entry group itself lacks the key.
    #[test]
    fn only_the_desktop_entry_group_is_read() {
        let file = parse(
            "[Desktop Entry]\nType=Application\n[Desktop Action x]\nName=Wrong\nIcon=wrong\n",
        )
        .expect("parses");
        assert_eq!(file.string("Name"), Ok(None));
        assert_eq!(file.string("Icon"), Ok(None));
    }

    /// The start group is the **first** group, so a file that opens with
    /// another group has a start group GIO refuses, even if a
    /// `[Desktop Entry]` follows.
    #[test]
    fn the_start_group_is_the_first_one_named() {
        let file = parse("[Other]\nA=1\n[Desktop Entry]\nName=X\n").expect("parses");
        assert_eq!(file.start_group(), Some("Other"));
        assert_eq!(file.string("Name"), Ok(Some("X".to_owned())));
    }

    /// A repeated group merges, and a repeated key keeps the last value.
    #[test]
    fn repeated_groups_merge_and_the_last_key_wins() {
        let file =
            parse("[Desktop Entry]\nName=A\n[X]\n[Desktop Entry]\nName=B\n").expect("parses");
        assert_eq!(file.string("Name"), Ok(Some("B".to_owned())));
    }

    /// **Every malformed line fails the whole file**, the way
    /// `g_key_file_load_from_file` does — GIO then lists no entry for it.
    #[test]
    fn a_malformed_line_fails_the_whole_file() {
        for (why, text) in [
            ("a bare word", "[Desktop Entry]\nName=X\nnonsense\n"),
            ("a line starting with =", "[Desktop Entry]\n=X\n"),
            ("a pair before any group", "Name=X\n[Desktop Entry]\n"),
            ("an unterminated group", "[Desktop Entry\nName=X\n"),
            ("text after a group's ]", "[Desktop Entry] x\nName=X\n"),
            ("an empty group name", "[]\n"),
            (
                "an indented line starting with =",
                "[Desktop Entry]\n\t=Y\n",
            ),
            ("a stray ] in a key", "[Desktop Entry]\nNa]me=X\n"),
            (
                "a key with an unclosed locale",
                "[Desktop Entry]\nName[sv=X\n",
            ),
            (
                "a key with text after its locale",
                "[Desktop Entry]\nName[sv]x=X\n",
            ),
            ("a locale with a space", "[Desktop Entry]\nName[s v]=X\n"),
            (
                "a non-UTF-8 encoding",
                "[Desktop Entry]\nEncoding=Legacy-Mixed\n",
            ),
            (
                "a malformed line in another group",
                "[Desktop Entry]\nName=X\n[Other]\n?\n",
            ),
        ] {
            assert!(parse(text).is_none(), "{why} must fail the file: {text:?}");
        }
    }

    /// **`Encoding=` binds the start group only** (`gkeyfile.c`'s
    /// `start_group == current_group` test): a legacy value in a later group
    /// is an ordinary key, while the same line in the start group fails the
    /// file (`a_malformed_line_fails_the_whole_file` covers that half).
    ///
    /// **Falsified** by checking `Encoding=` in every group (#1431 review,
    /// NIT 9).
    #[test]
    fn encoding_is_checked_in_the_start_group_only() {
        let file = parse("[Desktop Entry]\nName=X\n[Other]\nEncoding=Legacy-Mixed\n")
            .expect("a later group's Encoding= is just a key");
        assert_eq!(file.string("Name"), Ok(Some("X".to_owned())));
        assert!(parse("[Desktop Entry]\nEncoding=Legacy-Mixed\nName=X\n").is_none());
    }

    /// The lines `GLib` accepts that a naive parser might not.
    #[test]
    fn the_lines_glib_accepts() {
        for (why, text) in [
            ("an empty file", ""),
            (
                "a group with trailing blanks",
                "[Desktop Entry] \t\nName=X\n",
            ),
            ("indented lines", "  [Desktop Entry]\n\t# c\n\x0bName=X\n"),
            ("a space inside a key", "[Desktop Entry]\nX Key=1\n"),
            (
                "Encoding=UTF-8 in any case",
                "[Desktop Entry]\nEncoding=utf-8\n",
            ),
            ("an empty value", "[Desktop Entry]\nName=\n"),
            ("no trailing newline", "[Desktop Entry]\nName=X"),
            (
                "a locale with -_.@",
                "[Desktop Entry]\nName[sr_RS.UTF-8@latin]=X\n",
            ),
        ] {
            assert!(parse(text).is_some(), "{why} must parse: {text:?}");
        }
        let file = parse("[Desktop Entry]\nName=X").expect("parses");
        assert_eq!(file.string("Name"), Ok(Some("X".to_owned())));
    }

    /// Escapes decode; an unknown escape or a trailing `\` makes the value
    /// **invalid** (not absent), which is what GIO drops an entry for when it
    /// is the `Exec=` line.
    #[test]
    fn escapes_decode_and_bad_ones_are_invalid() {
        let file = parse("[Desktop Entry]\nA=a\\sb\\tc\\\\d\\ne\\rf\nB=bad\\q\nC=trailing\\\n")
            .expect("the file itself parses");
        assert_eq!(file.string("A"), Ok(Some("a b\tc\\d\ne\rf".to_owned())));
        assert_eq!(file.string("B"), Err(Invalid));
        assert_eq!(file.string("C"), Err(Invalid));
    }

    /// A value that is not UTF-8 is invalid, but does not fail the file.
    #[test]
    fn a_non_utf8_value_is_invalid() {
        let file = KeyFile::parse(b"[Desktop Entry]\nName=\xff\xfe\nIcon=ok\n").expect("parses");
        assert_eq!(file.string("Name"), Err(Invalid));
        assert_eq!(file.string("Icon"), Ok(Some("ok".to_owned())));
    }

    /// `g_key_file_get_locale_string`: the first language with a readable
    /// translation wins, an unreadable one is skipped, and the plain key is
    /// the fallback.
    #[test]
    fn a_localised_string_takes_the_first_readable_language() {
        let file = parse(
            "[Desktop Entry]\nName=Files\nName[sv]=Filer\nName[de]=Dateien\nName[fr]=bad\\q\n",
        )
        .expect("parses");
        let langs = |l: &[&str]| l.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            file.locale_string("Name", &langs(&["sv_SE", "sv"])),
            Some("Filer".to_owned())
        );
        assert_eq!(
            file.locale_string("Name", &langs(&["de", "sv"])),
            Some("Dateien".to_owned())
        );
        assert_eq!(
            file.locale_string("Name", &langs(&["fr", "sv"])),
            Some("Filer".to_owned())
        );
        assert_eq!(
            file.locale_string("Name", &langs(&["nb"])),
            Some("Files".to_owned())
        );
        assert_eq!(file.locale_string("Name", &[]), Some("Files".to_owned()));
        assert_eq!(file.locale_string("Icon", &langs(&["sv"])), None);
    }

    /// `true`/`1` (trailing whitespace ignored) are true; everything else,
    /// including `True`, is false — `GLib`'s own boolean parser.
    #[test]
    fn booleans_are_glibs() {
        let file = parse("[Desktop Entry]\nA=true\nB=1 \nC=True\nD=yes\nE=false\nF=truex\n")
            .expect("parses");
        assert!(file.boolean("A"));
        assert!(file.boolean("B"));
        for key in ["C", "D", "E", "F", "Missing"] {
            assert!(!file.boolean(key), "{key}");
        }
    }

    /// `g_ascii_isspace` is `GLib`'s own table (`glib/gstrfuncs.c:264`:
    /// entry `0x0b` is `0x004`, without `G_ASCII_SPACE`), so a vertical tab
    /// is not whitespace. A `\v`-led line is the key `"\vName"`, not `Name`,
    /// and a `\v` after `=` stays in the value.
    #[test]
    fn a_vertical_tab_is_not_glib_whitespace() {
        let file = parse("[Desktop Entry]\n\x0bName=X\nIcon=\x0bfoo\n").expect("parses");
        assert_eq!(file.string("Name"), Ok(None));
        assert_eq!(file.string("Icon"), Ok(Some("\x0bfoo".to_owned())));
    }

    /// `GLib` copies a value with `g_strndup` (`g_key_file_parse_key_value_pair`),
    /// which stops at a NUL byte.
    #[test]
    fn a_value_stops_at_a_nul_byte() {
        let file = parse("[Desktop Entry]\nName=T 24\0tail\n").expect("parses");
        assert_eq!(file.string("Name"), Ok(Some("T 24".to_owned())));
    }
}
