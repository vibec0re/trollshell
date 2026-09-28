//! What GIO does with a desktop entry's `Exec=` and `TryExec=` lines, ported
//! from the `GLib` the shell links (2.88.3).
//!
//! Two different readings of `Exec=`, and they are kept apart on purpose:
//!
//! - **Whether the entry exists at all** (`g_desktop_app_info_load_from_keyfile`):
//!   a non-empty `Exec=` must split as a shell command line
//!   (`g_shell_parse_argv`, ported as [`first_word`]) and its first word must
//!   name a program GIO can find (`g_find_program_for_path`, ported as
//!   [`find_program`]); the same for a non-empty `TryExec=`. An entry that
//!   fails either is absent from `g_app_info_get_all()`.
//! - **The executable layer 3 matches on** (`g_app_info_get_executable`):
//!   GIO's `binary_from_exec` — the first **space**-separated token of the
//!   line, taken literally. No quote removal, no `env FOO=1` skipping: an
//!   `Exec=env FOO=1 firefox` entry's executable is `env`, and one whose
//!   `Exec=` starts with a quoted path keeps the quote. [`binary`] does
//!   exactly that, so layer 3 compares what the native page compares.

use std::path::{Path, PathBuf};

/// `binary_from_exec`: skip leading spaces, then everything up to the next
/// space. Only called on an `Exec=` line that exists — GIO leaves the
/// executable `NULL` without one (see the parent module's gap list for what
/// the native page then does).
pub(super) fn binary(exec: &str) -> &str {
    let rest = exec.trim_start_matches(' ');
    rest.split(' ').next().unwrap_or(rest)
}

/// The first word of `command` as `g_shell_parse_argv` splits and unquotes
/// it, or `None` where that call fails: unbalanced quotes, a trailing `\`,
/// or no words at all.
pub(super) fn first_word(command: &str) -> Option<String> {
    let tokens = tokenize(command.as_bytes())?;
    let first = unquote(tokens.first()?)?;
    // Every byte `tokenize`/`unquote` drop or split at is ASCII, so what is
    // left of valid UTF-8 is still valid UTF-8.
    String::from_utf8(first).ok()
}

/// `tokenize_command_line` (`glib/gshell.c`), byte for byte: words split at
/// unquoted blanks and newlines, quotes and escapes kept in the word for
/// [`unquote`], and a `#` at the start of a word opening a comment to the
/// end of the line.
fn tokenize(b: &[u8]) -> Option<Vec<Vec<u8>>> {
    let mut quote = 0_u8;
    let mut quoted = false;
    let mut token: Option<Vec<u8>> = None;
    let mut tokens = Vec::new();
    let mut p = 0;
    while p < b.len() {
        let c = b[p];
        if quote == b'\\' {
            // A backslash-newline is nothing; anything else is kept, escape
            // and all, for `unquote`.
            if c != b'\n' {
                token.get_or_insert_with(Vec::new).extend([b'\\', c]);
            }
            quote = 0;
        } else if quote == b'#' {
            while p < b.len() && b[p] != b'\n' {
                p += 1;
            }
            quote = 0;
            if p == b.len() {
                break;
            }
        } else if quote != 0 {
            if c == quote && !(quote == b'"' && quoted) {
                quote = 0;
            }
            token.get_or_insert_with(Vec::new).push(c);
        } else {
            match c {
                b'\n' => tokens.extend(token.take()),
                b' ' | b'\t' => {
                    if token.as_ref().is_some_and(|t| !t.is_empty()) {
                        tokens.extend(token.take());
                    }
                }
                b'\'' | b'"' => {
                    token.get_or_insert_with(Vec::new).push(c);
                    quote = c;
                }
                b'\\' => quote = c,
                b'#' if p == 0 || matches!(b[p - 1], b' ' | b'\n' | 0) => quote = c,
                _ => token.get_or_insert_with(Vec::new).push(c),
            }
        }
        // Consecutive backslashes counted mod 2, to tell an escaped `"`.
        quoted = b[p] == b'\\' && !quoted;
        p += 1;
    }
    tokens.extend(token);
    if quote != 0 || tokens.is_empty() {
        return None;
    }
    Some(tokens)
}

/// `g_shell_unquote`: backslash escapes outside quotes, `'…'` literal,
/// `"…"` with `\"`, `\\`, `` \` ``, `\$` and backslash-newline escaped.
fn unquote(s: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        match s[i] {
            b'\\' => {
                i += 1;
                if let Some(&c) = s.get(i) {
                    if c != b'\n' {
                        out.push(c);
                    }
                    i += 1;
                }
            }
            b'\'' => {
                let close = s[i + 1..].iter().position(|&c| c == b'\'')?;
                out.extend_from_slice(&s[i + 1..i + 1 + close]);
                i += close + 2;
            }
            b'"' => {
                i += 1;
                loop {
                    match *s.get(i)? {
                        b'"' => {
                            i += 1;
                            break;
                        }
                        b'\\' => {
                            i += 1;
                            match s.get(i) {
                                Some(&c @ (b'"' | b'\\' | b'`' | b'$' | b'\n')) => {
                                    out.push(c);
                                    i += 1;
                                }
                                // Not an escape: the backslash stays, and
                                // the next byte is read on its own.
                                _ => out.push(b'\\'),
                            }
                        }
                        c => {
                            out.push(c);
                            i += 1;
                        }
                    }
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    Some(out)
}

/// `g_find_program_for_path(program, NULL, working_dir)`: whether `program`
/// names an executable, non-directory file — directly when it is absolute
/// or contains a `/`, else in each directory of `path` (an empty entry
/// meaning the current directory). `working_dir` is the entry's `Path=`
/// key: a relative candidate is resolved against it first, as GIO does.
pub(super) fn find_program(program: &str, path: &[PathBuf], working_dir: Option<&str>) -> bool {
    let original = Path::new(program);
    let direct = match working_dir {
        Some(dir) if !original.is_absolute() => Path::new(dir).join(original),
        _ => original.to_path_buf(),
    };
    if direct.is_absolute() || program.contains('/') {
        if is_executable(&direct) {
            return true;
        }
        if original.is_absolute() {
            return false;
        }
    }
    path.iter().any(|dir| {
        let candidate = dir.join(original);
        let candidate = match working_dir {
            Some(wd) if !candidate.is_absolute() => Path::new(wd).join(candidate),
            _ => candidate,
        };
        is_executable(&candidate)
    })
}

/// `g_file_test(…, G_FILE_TEST_IS_EXECUTABLE) && !g_file_test(…, IS_DIR)`:
/// `access(X_OK)` for a non-root caller, and not a directory.
fn is_executable(path: &Path) -> bool {
    nix::unistd::access(path, nix::unistd::AccessFlags::X_OK).is_ok() && !path.is_dir()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use super::{binary, find_program, first_word};

    /// **The executable is GIO's `binary_from_exec`**, not a shell parse: the
    /// first space-separated token, literally.
    #[test]
    fn the_executable_is_the_first_space_separated_token() {
        assert_eq!(binary("firefox %U"), "firefox");
        assert_eq!(
            binary("   /usr/bin/firefox --new-window"),
            "/usr/bin/firefox"
        );
        assert_eq!(binary("env FOO=1 firefox"), "env", "no env-prefix skipping");
        assert_eq!(binary("\"/opt/My App/app\" %f"), "\"/opt/My");
        assert_eq!(binary("a\tb"), "a\tb", "only spaces separate");
        assert_eq!(binary(""), "");
    }

    /// `g_shell_parse_argv`'s first word: quotes and escapes removed.
    #[test]
    fn the_first_word_is_shell_parsed() {
        assert_eq!(first_word("firefox %U").as_deref(), Some("firefox"));
        assert_eq!(
            first_word("  \t'/opt/My App/app' %f").as_deref(),
            Some("/opt/My App/app")
        );
        assert_eq!(
            first_word("\"/opt/a \\\"b\\\"\" x").as_deref(),
            Some("/opt/a \"b\"")
        );
        assert_eq!(first_word("my\\ app x").as_deref(), Some("my app"));
        assert_eq!(
            first_word("\"a\\qb\"").as_deref(),
            Some("a\\qb"),
            "an unknown escape keeps its backslash"
        );
        assert_eq!(first_word("#comment\nreal arg").as_deref(), Some("real"));
        assert_eq!(
            first_word("a#b").as_deref(),
            Some("a#b"),
            "# only opens a comment at a word start"
        );
    }

    /// Where `g_shell_parse_argv` fails, and GIO drops the entry.
    #[test]
    fn a_command_line_the_shell_cannot_split_has_no_first_word() {
        for bad in [
            "",
            "   ",
            "\t\n",
            "'unclosed",
            "\"unclosed",
            "trailing\\",
            "#",
        ] {
            assert_eq!(first_word(bad), None, "{bad:?}");
        }
    }

    fn executable(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, "#!/bin/sh\n").expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    /// `g_find_program_for_path` over a fixture `PATH`: a bare name is looked
    /// up in each directory, a path is tested directly, and a file that is
    /// not executable or is a directory does not count.
    #[test]
    fn a_program_is_found_the_way_gio_finds_it() {
        let root = tempfile::tempdir().expect("tempdir");
        let bin = root.path().join("bin");
        let other = root.path().join("other");
        std::fs::create_dir_all(&bin).expect("mkdir");
        std::fs::create_dir_all(other.join("adir")).expect("mkdir");
        let firefox = executable(&bin, "firefox");
        std::fs::write(bin.join("plain"), "").expect("write");
        let path = [other.clone(), bin.clone()];

        assert!(
            find_program("firefox", &path, None),
            "found in the second PATH entry"
        );
        assert!(
            !find_program("firefox", std::slice::from_ref(&other), None),
            "not on this PATH"
        );
        assert!(!find_program("plain", &path, None), "not executable");
        assert!(!find_program("adir", &path, None), "a directory");
        assert!(
            find_program(firefox.to_str().unwrap(), &[], None),
            "an absolute path needs no PATH"
        );
        assert!(
            !find_program("/nonexistent/firefox", &path, None),
            "an absolute miss is final"
        );
        assert!(
            find_program("bin/firefox", &[], Some(root.path().to_str().unwrap())),
            "a relative path is resolved against Path=",
        );
        assert!(
            find_program("firefox", &[], Some(bin.to_str().unwrap())),
            "a bare name is tried in Path= before PATH",
        );
    }
}
