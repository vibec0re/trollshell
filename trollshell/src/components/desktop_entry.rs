//! Desktop-entry knowledge: what `Exec=` means, and which entries a user may
//! pick from — #1071 §3.2 and §5, phase 4.
//!
//! Two consumers, which is why this is a `components/` module rather than a
//! private helper of either:
//!
//! * [`crate::workspace_stacks`] resolves a stack app's desktop-entry id to the
//!   command that starts it ([`launchable`], [`exec_words`],
//!   [`strip_field_codes`]). Until phase 4 an app with no `exec` override was
//!   launched by running its **id** as a command, so `id = "org.mozilla.firefox"`
//!   started nothing at all (#1106's own body says so).
//! * [`crate::components::app_picker`] lists the entries the Edit sub-page's
//!   **Add app** offers ([`installed`], [`filtered`]).
//!
//! ## Why the lookup is exact, unlike `app_meta`'s
//!
//! [`crate::components::app_meta::resolve_app_meta`] resolves an `app_id` to an
//! icon through three layers, two of them fuzzy — case-insensitive containment
//! and an executable-basename match — because a *wrong* icon is a cosmetic
//! problem and no icon at all is worse. Launching is the opposite: starting
//! Firefox Developer Edition because the stack said `firefox` is a real, visible
//! wrong answer that the user then has to notice and undo. So this resolves an
//! id **exactly** (with one lowercase retry, which is a spelling of the same id
//! rather than a different entry), and an id that names no entry is reported —
//! §3.2's *"a desktop id with no entry and no override → one warning naming it,
//! Start continues with the rest"* — rather than guessed at.
//!
//! ## Why `glib::KeyFile` and not `gio::AppInfo`
//!
//! `DBusActivatable` is not on the `gio::AppInfo` *interface*, and
//! `gio::DesktopAppInfo` — which does expose it — is absent from the gio 0.22
//! bindings this workspace vendors (`app_meta`'s and `widgets::calendar`'s docs
//! record the same constraint). `glib::KeyFile::load_from_data_dirs` reads the
//! entry out of `$XDG_DATA_HOME` then `$XDG_DATA_DIRS` — the same search path,
//! the same file — and hands back every key, so one read answers both `Exec=`
//! and `DBusActivatable=`.
//!
//! It is also **plain file IO**: no `GObject`, no main-loop affinity. That is what
//! lets `workspace_stacks`' Start transaction resolve an entry from the tokio
//! runtime without hopping to the GTK thread. Only the *activation* of a
//! `DBusActivatable` entry needs the main thread, because that one really does
//! go through `gio::AppInfo::launch` — see [`crate::workspace_stacks::Live`].

use hytte::gtk::{gio, glib, prelude::*};

/// The `[Desktop Entry]` group every key below lives in.
const GROUP: &str = "Desktop Entry";

/// What a Start needs to know about a desktop entry (#1071 §3.2).
///
/// Plain data on purpose — no `gio::Icon`, no `GObject` of any kind — so it
/// crosses from the GTK thread to the tokio runtime and back through the
/// `workspace_stacks::Ops` seam like every other value there.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Launchable {
    /// The entry's `Exec=` line **verbatim**, field codes and all. Stripping
    /// them is [`strip_field_codes`]'s job, and it is deliberately not done
    /// here: what the file says and what we choose to run are two different
    /// facts, and a test that wants to see a `%U` dropped needs to be able to
    /// state one without the other.
    pub exec: String,
    /// `DBusActivatable=true` — the entry is started by asking its own D-Bus
    /// name to activate, not by running [`Self::exec`] (#1071 §3.2).
    pub dbus_activatable: bool,
}

/// The desktop entry installed under the id `id`, if there is exactly one.
///
/// `id` is what niri reports as a window's `app_id` and what `workspaces.toml`
/// stores — `org.mozilla.firefox`, `Alacritty` — **without** the `.desktop`
/// suffix, which is added here.
///
/// Two spellings are tried, in order: the id as given, then its lowercase form.
/// The second is not a fuzzy match — `Alacritty` and `alacritty` are two
/// spellings of one id, and which one a distribution ships the file under is not
/// something a `workspaces.toml` author should have to know. Nothing else is
/// tried; see the module doc for why launching does not guess.
///
/// An entry with no `Exec=` at all (a `Type=Link` entry, say) answers `None`:
/// there is nothing to launch, which is the same answer as "no such entry" as
/// far as every caller is concerned.
///
/// Reads the filesystem. Safe on any thread — see the module doc.
pub(crate) fn launchable(id: &str) -> Option<Launchable> {
    let lower = id.to_lowercase();
    let mut spellings = vec![id.to_owned()];
    if lower != id {
        spellings.push(lower);
    }
    for spelling in spellings {
        let key_file = glib::KeyFile::new();
        let relative = format!("applications/{spelling}.desktop");
        if key_file
            .load_from_data_dirs(&relative, glib::KeyFileFlags::NONE)
            .is_err()
        {
            continue;
        }
        let Ok(exec) = key_file.string(GROUP, "Exec") else {
            continue;
        };
        return Some(Launchable {
            exec: exec.to_string(),
            // `.unwrap_or(false)` rather than a reported error: an absent
            // `DBusActivatable` key is the overwhelmingly common case and means
            // exactly `false`, and a malformed one ("yes") is a slip in someone
            // else's package that must not stop the app launching the ordinary
            // way.
            dbus_activatable: key_file.boolean(GROUP, "DBusActivatable").unwrap_or(false),
        });
    }
    None
}

/// Split an `Exec=` line — or a user's own launch-command override — into argv.
///
/// Shell-style quoting, because that is what `GLib` itself applies to an `Exec`
/// line (`g_shell_parse_argv`, reached through `g_desktop_app_info`'s parameter
/// expansion): single quotes are literal, double quotes allow `\` escapes, and a
/// backslash outside quotes escapes the next character. A plain
/// `split_whitespace` — which is what phase 2 used for the `exec` override — cuts
/// `sh -c 'exec foo "$@"'` into five broken words, and wrapper entries of exactly
/// that shape are the norm on this distribution.
///
/// **An unterminated quote closes at the end of the string** rather than
/// producing an error. `GLib`'s parser refuses such a line outright; refusing here
/// would mean a stack app silently not launching because the user typed one
/// stray `"` in the Edit form, and launching the obvious reading of what they
/// typed is the better failure. There is no error path out of this function by
/// design.
pub(crate) fn exec_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut chars = line.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            c if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            '\'' => {
                started = true;
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    word.push(c);
                }
            }
            '"' => {
                started = true;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        // Inside double quotes only these four are escapable;
                        // every other backslash is a literal backslash, which is
                        // what both the Desktop Entry spec and the shell say.
                        '\\' => match chars.peek() {
                            Some(&next @ ('"' | '\\' | '$' | '`')) => {
                                word.push(next);
                                chars.next();
                            }
                            _ => word.push('\\'),
                        },
                        _ => word.push(c),
                    }
                }
            }
            '\\' => {
                started = true;
                if let Some(next) = chars.next() {
                    word.push(next);
                }
            }
            c => {
                started = true;
                word.push(c);
            }
        }
    }
    if started {
        words.push(word);
    }
    words
}

/// The field codes #1071 §3.2 names, in the spelling they appear in after a `%`.
///
/// `%f`/`%F`/`%u`/`%U` are the file and URI arguments a launcher would
/// substitute, `%i` expands to `--icon <icon>`, `%c` to the entry's name and
/// `%k` to its path. A stack app is launched with **no** document and no launch
/// context, so every one of them expands to nothing — which is precisely why
/// they have to be *removed* rather than passed through as the literal two
/// characters, which is what a naive split would hand to `execve`.
const FIELD_CODES: [char; 7] = ['f', 'F', 'u', 'U', 'i', 'c', 'k'];

/// Drop the field codes from an already-split `Exec` argv (#1071 §3.2).
///
/// * `%%` becomes a literal `%` — the spec's own escape, and the only way an
///   `Exec` line can carry a percent sign at all.
/// * each of [`FIELD_CODES`] expands to nothing.
/// * **any other** `%x` is left exactly as it was, including the `%`. The
///   Desktop Entry spec deprecated `%d %D %n %N %v %m` and says a reader may
///   ignore them; silently eating an unknown code would instead turn a typo in
///   someone's entry into a *missing argument*, which is far harder to see than
///   a literal `%q` in a failed command line.
///
/// A word that consisted **only** of field codes disappears entirely, rather
/// than becoming an empty argument — `foo %U` is `["foo"]`, not `["foo", ""]`,
/// and an empty argv slot is a real difference to most programs. A word that had
/// other characters keeps its place even if it shrinks (`--file=%f` →
/// `--file=`), because dropping it would silently change the meaning of the
/// flags around it.
pub(crate) fn strip_field_codes(words: &[String]) -> Vec<String> {
    words
        .iter()
        .filter_map(|word| {
            let mut out = String::new();
            let mut chars = word.chars().peekable();
            let mut dropped_any = false;
            while let Some(ch) = chars.next() {
                if ch != '%' {
                    out.push(ch);
                    continue;
                }
                match chars.peek().copied() {
                    Some('%') => {
                        out.push('%');
                        chars.next();
                    }
                    Some(code) if FIELD_CODES.contains(&code) => {
                        dropped_any = true;
                        chars.next();
                    }
                    // A trailing lone `%`, or an unknown code: verbatim.
                    _ => out.push('%'),
                }
            }
            // Only a word that was *emptied by a field code* disappears. A word
            // that was empty to begin with — `foo "" bar`, a deliberate empty
            // argument — is not this function's business.
            if out.is_empty() && dropped_any {
                None
            } else {
                Some(out)
            }
        })
        .collect()
}

/// Start a `DBusActivatable=true` entry through GIO (#1071 §3.2).
///
/// **GTK main thread only.** `gio::AppInfo` is a `GObject` interface, not `Send`,
/// and this is the one call in the epic that genuinely needs the main loop —
/// `workspace_stacks::Live::activate` hops here from the runtime and says why.
///
/// `AppInfo::launch` is what carries GIO's own D-Bus-activation walk (desktop id
/// → bus name → `org.freedesktop.Application.Activate`) *and* its fallback to
/// the entry's `Exec` when the activation fails. Hand-rolling that over
/// `hytte-bus` would be a second implementation of someone else's spec.
///
/// The lookup is by **exact** id, for the module doc's reason: launching does
/// not guess.
///
/// # Errors
/// A missing entry, or whatever GIO said about the launch.
pub(crate) fn activate(id: &str) -> Result<(), String> {
    let wanted = format!("{id}.desktop");
    let found = gio::AppInfo::all()
        .into_iter()
        .find(|info| info.id().is_some_and(|got| got == wanted.as_str()));
    match found {
        Some(info) => info
            .launch(&[], gio::AppLaunchContext::NONE)
            .map_err(|e| format!("activating {id}: {e}")),
        None => Err(format!("{id} has no desktop entry to activate")),
    }
}

/// One row the **Add app** picker can offer (#1071 §5).
///
/// Split from `gio::AppInfo` so the picker's filtering is a pure function of
/// plain data: `AppInfo` is a `GObject` interface with no constructor a test can
/// reach, so a picker that filtered `AppInfo`s directly could only be falsified
/// against whatever happens to be installed on the machine running the suite.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PickerEntry {
    /// The desktop id **without** `.desktop` — what goes into `workspaces.toml`
    /// as a stack app's `id`, and what niri reports as the window's `app_id`.
    pub id: String,
    /// The entry's display name, as shown in the list.
    pub name: String,
    /// `gio::AppInfo::should_show()` — `false` for a `NoDisplay=true` entry and
    /// for one this desktop environment is excluded from by `OnlyShowIn=` /
    /// `NotShowIn=`. §5 asks for the list "filtered to `NoDisplay=false`"; GIO's
    /// own predicate is that plus the two environment keys, which is the same
    /// question ("would a menu show this?") asked completely.
    pub show: bool,
}

/// Every installed application, as picker rows.
///
/// Reads `gio::AppInfo::all()`, so it must run on the GTK main thread — and it
/// deliberately does **not** filter: [`filtered`] does, from plain data, where a
/// test can see it happen. Entries with no id at all are dropped here, since an
/// id is the whole thing being picked.
pub(crate) fn installed() -> Vec<PickerEntry> {
    let mut rows: Vec<PickerEntry> = gio::AppInfo::all()
        .into_iter()
        .filter_map(|info| {
            let id = info.id()?;
            let id = id.strip_suffix(".desktop").unwrap_or(id.as_str());
            (!id.is_empty()).then(|| PickerEntry {
                id: id.to_owned(),
                name: info.display_name().to_string(),
                show: info.should_show(),
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.id.cmp(&b.id))
    });
    rows
}

/// The rows a picker showing `query` should list (#1071 §5).
///
/// Two filters, and the order matters only for reading: a hidden entry is never
/// offered whatever the query says, and an empty query narrows nothing. The
/// match is case-insensitive substring over the display **name** and the **id**,
/// because a user who knows a program as `org.gnome.Nautilus` should not have to
/// remember that its name is "Files".
pub(crate) fn filtered<'e>(entries: &'e [PickerEntry], query: &str) -> Vec<&'e PickerEntry> {
    let needle = query.trim().to_lowercase();
    entries
        .iter()
        .filter(|entry| entry.show)
        .filter(|entry| {
            needle.is_empty()
                || entry.name.to_lowercase().contains(&needle)
                || entry.id.to_lowercase().contains(&needle)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{PickerEntry, exec_words, filtered, strip_field_codes};

    fn words(line: &str) -> Vec<String> {
        exec_words(line)
    }

    /// The argv a stack app is launched with, end to end: split, then stripped.
    fn resolved(line: &str) -> Vec<String> {
        strip_field_codes(&exec_words(line))
    }

    #[test]
    fn a_plain_exec_line_splits_on_whitespace() {
        assert_eq!(words("alacritty -e weechat"), ["alacritty", "-e", "weechat"]);
        assert_eq!(words("   firefox   "), ["firefox"]);
        assert_eq!(words(""), Vec::<String>::new());
    }

    /// `GLib` parses an `Exec` line with `g_shell_parse_argv`, so a quoted
    /// argument is **one** argument. `sh -c '…'` wrapper entries are the norm
    /// here, and `split_whitespace` (what phase 2 used) breaks every one.
    #[test]
    fn quotes_group_one_argument() {
        assert_eq!(
            words("sh -c 'exec foo --bar baz'"),
            ["sh", "-c", "exec foo --bar baz"]
        );
        assert_eq!(
            words(r#"alacritty -e "weechat --home /tmp""#),
            ["alacritty", "-e", "weechat --home /tmp"]
        );
        // A backslash escape inside double quotes, and a literal one outside.
        assert_eq!(words(r#""a\"b" c\ d"#), [r#"a"b"#, "c d"]);
        // Single quotes are literal: a `$` inside them is not special here
        // because nothing in this path ever reaches a shell.
        assert_eq!(words(r#"sh -c 'echo "$@"' x"#), ["sh", "-c", r#"echo "$@""#, "x"]);
    }

    /// Documented: an unterminated quote closes at the end rather than erroring,
    /// so one stray `"` in the Edit form does not turn into "nothing launched".
    #[test]
    fn an_unterminated_quote_closes_at_the_end() {
        assert_eq!(words(r#"foo "bar baz"#), ["foo", "bar baz"]);
        assert_eq!(words("foo 'bar"), ["foo", "bar"]);
    }

    /// #1071 §3.2's table: `%u %U %f %F %i %c %k` dropped, `%%` unescaped.
    ///
    /// Falsified by keeping any one of them — see the PR's mutation table.
    #[test]
    fn every_named_field_code_is_dropped() {
        for code in ["%u", "%U", "%f", "%F", "%i", "%c", "%k"] {
            assert_eq!(
                resolved(&format!("firefox {code}")),
                ["firefox"],
                "{code} survived the strip"
            );
        }
        // All of them at once, and interleaved with real arguments.
        assert_eq!(
            resolved("prog %f --flag %U value %i %c %k"),
            ["prog", "--flag", "value"]
        );
    }

    #[test]
    fn a_double_percent_becomes_one() {
        assert_eq!(resolved("prog 100%% done"), ["prog", "100%", "done"]);
        // `%%u` is an escaped percent followed by a literal `u`, NOT the `%u`
        // field code — the escape is consumed first.
        assert_eq!(resolved("prog %%u"), ["prog", "%u"]);
    }

    /// An unknown or deprecated code keeps its `%`, so a typo shows up in the
    /// failed command line instead of silently removing an argument.
    #[test]
    fn an_unknown_field_code_is_left_verbatim() {
        assert_eq!(resolved("prog %q %d %m"), ["prog", "%q", "%d", "%m"]);
        assert_eq!(resolved("prog 50%"), ["prog", "50%"]);
    }

    /// A word that was *only* a field code disappears; one that merely shrinks
    /// keeps its place, because an empty argv slot is a real difference.
    #[test]
    fn a_word_emptied_by_a_field_code_disappears_but_a_shrunken_one_stays() {
        assert_eq!(resolved("prog %F"), ["prog"]);
        assert_eq!(resolved("prog --file=%f"), ["prog", "--file="]);
        // A deliberately empty argument is not a field code's doing and stays.
        assert_eq!(resolved(r#"prog "" x"#), ["prog", "", "x"]);
    }

    fn entry(id: &str, name: &str, show: bool) -> PickerEntry {
        PickerEntry {
            id: id.to_owned(),
            name: name.to_owned(),
            show,
        }
    }

    fn ids(rows: &[&PickerEntry]) -> Vec<String> {
        rows.iter().map(|e| e.id.clone()).collect()
    }

    /// §5: the picker lists entries "filtered to `NoDisplay=false`".
    ///
    /// Falsified by dropping the `show` filter — see the PR's mutation table.
    #[test]
    fn a_hidden_entry_is_never_offered() {
        let entries = [
            entry("org.mozilla.firefox", "Firefox", true),
            entry("nvidia-settings", "NVIDIA Settings", false),
            entry("Alacritty", "Alacritty", true),
        ];
        assert_eq!(
            ids(&filtered(&entries, "")),
            ["org.mozilla.firefox", "Alacritty"]
        );
        // …and not even when the query names it exactly.
        assert!(filtered(&entries, "NVIDIA").is_empty());
    }

    #[test]
    fn the_search_narrows_by_name_and_by_id() {
        let entries = [
            entry("org.mozilla.firefox", "Firefox", true),
            entry("org.gnome.Nautilus", "Files", true),
            entry("Alacritty", "Alacritty", true),
        ];
        assert_eq!(ids(&filtered(&entries, "fire")), ["org.mozilla.firefox"]);
        // Case-insensitive.
        assert_eq!(ids(&filtered(&entries, "FIRE")), ["org.mozilla.firefox"]);
        // By id, for a program whose display name shares nothing with it.
        assert_eq!(ids(&filtered(&entries, "nautilus")), ["org.gnome.Nautilus"]);
        // By name, for the same entry.
        assert_eq!(ids(&filtered(&entries, "files")), ["org.gnome.Nautilus"]);
        // Surrounding whitespace is the user still typing, not a narrowing.
        assert_eq!(ids(&filtered(&entries, "  ")).len(), 3);
        assert!(filtered(&entries, "no-such-app").is_empty());
    }
}
