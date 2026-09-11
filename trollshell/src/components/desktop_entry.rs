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

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use hytte::gtk::{gio, glib, prelude::*};

use crate::components::app_meta::{AppMeta, MetaCache};

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
    /// `DBusActivatable=true` — the entry *asks* to be started by activating
    /// its own D-Bus name rather than by running [`Self::exec`] (#1071 §3.2).
    ///
    /// Asking is not enough on its own: see [`is_valid_bus_name`] for why the
    /// id has to be a usable bus name before this is honoured.
    pub dbus_activatable: bool,
    /// `TryExec` named a program that is not on `PATH`.
    ///
    /// GIO refuses to construct a `GDesktopAppInfo` at all in this case, which
    /// is why such an entry never reaches the picker — `installed()` goes
    /// through `AppInfo::all()`. This reader goes at the keyfile directly, so
    /// without this it would happily hand back an `Exec` for a program that is
    /// not installed, and the Start would surface as a unit *start failure*
    /// rather than as §3.2's "nothing to start" warning naming the app.
    pub try_exec_missing: bool,
}

/// Whether `id` can be used as a D-Bus well-known name (#1071 §3.2, review
/// MEDIUM 7).
///
/// This is the gate on D-Bus activation, and it is not decoration. GIO takes the
/// activation path in `g_desktop_app_info_launch_uris_internal` only when there
/// is a session bus **and** the entry's derived app id is a valid bus name;
/// otherwise it silently falls through to `launch_uris_with_spawn`, which — as
/// `workspace_stacks::may_stop`'s own measured doc records — forks the child
/// into **`trollshell.service`'s own cgroup**. So an entry that says
/// `DBusActivatable=true` under a one-element id like `Alacritty` would not be
/// activated at all; it would be forked under the shell, outside any slice,
/// dying with the next `systemctl --user restart trollshell`.
///
/// Checking here means such an entry takes our ordinary launcher into the
/// stack's own slice instead, which is both what the user expects and what Stop
/// can actually take down.
///
/// The rule is D-Bus's own for a well-known name: two or more elements separated
/// by `.`, each element one or more of `[A-Za-z_-][A-Za-z0-9_-]*` (so no element
/// may start with a digit), at most 255 bytes, and no unique-name `:` prefix.
#[must_use]
pub(crate) fn is_valid_bus_name(id: &str) -> bool {
    if id.is_empty() || id.len() > 255 || id.starts_with(':') {
        return false;
    }
    let elements: Vec<&str> = id.split('.').collect();
    if elements.len() < 2 {
        return false;
    }
    elements.iter().all(|element| {
        let mut chars = element.chars();
        let Some(first) = chars.next() else {
            // An empty element — a leading, trailing or doubled dot.
            return false;
        };
        (first.is_ascii_alphabetic() || first == '_' || first == '-')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    })
}

/// The file names an id may be installed under, in the order they are tried.
///
/// The id as given, then its lowercase form. The second is **not** a fuzzy
/// match — `Alacritty` and `alacritty` are two spellings of one id, and which
/// one a distribution ships the file under is not something a `workspaces.toml`
/// author should have to know. (`Alacritty` is the epic's own example, so this
/// retry is load-bearing rather than defensive.)
///
/// Empty for an id that must never be interpolated into a path: one containing
/// `/` would traverse out of `applications/`, and `.` alone or `..` would name a
/// directory. Ids are not validated anywhere else — `is_valid_workspace_name`
/// governs stack *names*, and an ephemeral Save takes whatever `app_id` a window
/// reports — so the guard belongs here, at the one place an id becomes a path.
#[must_use]
pub(crate) fn spellings(id: &str) -> Vec<String> {
    if id.is_empty() || id.contains('/') || id.contains('\\') || id.split('.').any(|p| p.is_empty())
    {
        return Vec::new();
    }
    let lower = id.to_lowercase();
    if lower == id {
        vec![id.to_owned()]
    } else {
        vec![id.to_owned(), lower]
    }
}

/// Whether an entry's `TryExec` value permits launching it.
///
/// `None` (no `TryExec` key) is always fine. A relative name is looked up on
/// `PATH`; an absolute path is taken as given. `on_path` is injected so the
/// decision is testable without depending on what happens to be installed on
/// the machine running the suite.
fn try_exec_ok(value: Option<&str>, on_path: impl Fn(&str) -> bool) -> bool {
    match value.map(str::trim).filter(|v| !v.is_empty()) {
        None => true,
        Some(program) => on_path(program),
    }
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
    for spelling in spellings(id) {
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
        let try_exec = key_file.string(GROUP, "TryExec").ok();
        return Some(Launchable {
            exec: exec.to_string(),
            // `.unwrap_or(false)` rather than a reported error: an absent
            // `DBusActivatable` key is the overwhelmingly common case and means
            // exactly `false`, and a malformed one ("yes") is a slip in someone
            // else's package that must not stop the app launching the ordinary
            // way.
            dbus_activatable: key_file.boolean(GROUP, "DBusActivatable").unwrap_or(false),
            try_exec_missing: !try_exec_ok(try_exec.as_deref().map(str::trim), |program| {
                glib::find_program_in_path(program).is_some()
            }),
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
const FIELD_CODES: [char; 13] = [
    'f', 'F', 'u', 'U', 'i', 'c', 'k', // …and the deprecated set (review LOW 11).
    //
    // The Desktop Entry spec says a **deprecated** code must be *removed*, not
    // ignored in place — they are `%d` `%D` `%n` `%N` (the directory and file
    // names a launcher used to substitute) and `%v` `%m` (the device and a
    // legacy mini-icon). §3.2's list omits them, but leaving them to the
    // "unknown code, keep it verbatim" arm below hands an old entry's program a
    // literal `%d` argument, which is the failure that arm exists to *avoid*
    // for genuinely unknown codes. Known-and-dead is not unknown.
    'd', 'D', 'n', 'N', 'v', 'm',
];

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
/// The lookup is by **exact** id, for the module doc's reason: launching does
/// not guess.
///
/// # What `Ok` means here, precisely
///
/// **"GIO accepted the request"**, and no more than that. On the D-Bus path GIO
/// issues an *async* `org.freedesktop.Application.Activate` and returns without
/// waiting for the reply, so an entry whose bus name is not actually activatable
/// still answers `Ok`. There is therefore no "the activation failed, warn and
/// carry on" branch to be had from this return value, and the shell should not
/// pretend otherwise — the app simply never appears, and §3.4's grace window
/// names it in the missing-apps line like any other app that did not arrive.
///
/// The `Err` arm is in practice reachable only for "no entry found at all".
///
/// # Why the caller must check the bus name first
///
/// GIO takes the D-Bus path only when the derived app id is a valid bus name;
/// otherwise it falls through to `launch_uris_with_spawn`, which forks the child
/// into **this shell's own cgroup** (measured, and recorded on
/// `workspace_stacks::may_stop`). So this must only ever be called for an id
/// [`is_valid_bus_name`] accepts — `workspace_stacks::app_start` is what enforces
/// that, and routes everything else through the ordinary launcher into the
/// stack's slice.
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

/// Every installed application, as picker rows — **plus a pre-filled icon
/// cache**, from a single `gio::AppInfo::all()` scan.
///
/// Reads `gio::AppInfo::all()`, so it must run on the GTK main thread — and it
/// deliberately does **not** filter: [`filtered`] does, from plain data, where a
/// test can see it happen. Entries with no id at all are dropped here, since an
/// id is the whole thing being picked.
///
/// # Why it returns the cache too (review MEDIUM 6)
///
/// `resolve_app_meta` scans `AppInfo::all()` on a **cache miss**, and walks it up
/// to three times. With a fresh cache every offered row is a miss, so rendering
/// N rows cost N full scans — measured at one scan per row, 25 ms for six rows
/// against a ~4 ms scan, and a desktop carries hundreds of entries.
///
/// The fix is not to make the lookup cheaper but to stop doing it: this function
/// already holds every `AppInfo` in its hand, so it fills the cache the rows
/// will consult as it goes. Every row is then a hit and the whole picker costs
/// **one** scan. `PickerEntry` stays plain data (no `gio::Icon` field), so the
/// filtering it feeds remains testable without a display.
pub(crate) fn installed() -> (Vec<PickerEntry>, MetaCache) {
    let all = gio::AppInfo::all();
    let cache: MetaCache = Rc::new(RefCell::new(HashMap::with_capacity(all.len())));
    let mut rows: Vec<PickerEntry> = Vec::with_capacity(all.len());
    {
        let mut cache = cache.borrow_mut();
        for info in all {
            let Some(full_id) = info.id() else { continue };
            let id = full_id.strip_suffix(".desktop").unwrap_or(full_id.as_str());
            if id.is_empty() {
                continue;
            }
            let name = info.display_name().to_string();
            // Keyed by the id the row will ask for, so `resolve_app_meta`'s
            // first lookup is a hit and it never reaches its own scan.
            cache.insert(
                id.to_owned(),
                Some(AppMeta {
                    display_name: name.clone(),
                    icon: info.icon(),
                }),
            );
            rows.push(PickerEntry {
                id: id.to_owned(),
                name,
                show: info.should_show(),
            });
        }
    }
    rows.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.id.cmp(&b.id))
    });
    (rows, cache)
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
    use super::{
        PickerEntry, exec_words, filtered, is_valid_bus_name, spellings, strip_field_codes,
        try_exec_ok,
    };

    /// Review LOW 9 / the reviewer's surviving **M9**: the lowercase retry is
    /// the one behaviour the module doc argues for at length, and `Alacritty` is
    /// the epic's own example id — but `launchable` reaches `$XDG_DATA_DIRS`
    /// directly, so the retry could not be tested until it was split out.
    ///
    /// **The mutation**: dropping the lowercase spelling reds this. It survived
    /// the reviewer's whole suite before the split.
    #[test]
    fn an_id_is_tried_as_written_and_then_lowercased() {
        assert_eq!(spellings("Alacritty"), ["Alacritty", "alacritty"]);
        // Already lowercase: one spelling, not the same one twice — a duplicate
        // would double the filesystem work on the common path.
        assert_eq!(spellings("firefox"), ["firefox"]);
        assert_eq!(spellings("org.mozilla.firefox"), ["org.mozilla.firefox"]);
        assert_eq!(
            spellings("org.gnome.Nautilus"),
            ["org.gnome.Nautilus", "org.gnome.nautilus"]
        );
    }

    /// Review LOW 13: an id becomes a path, so one that could traverse out of
    /// `applications/` is refused rather than interpolated. Ids are validated
    /// nowhere else — an ephemeral Save takes whatever `app_id` a window reports.
    #[test]
    fn an_id_that_would_traverse_is_refused_outright() {
        for hostile in [
            "../../etc/passwd",
            "a/b",
            "..",
            ".",
            "",
            "a..b",
            ".leading",
            "trailing.",
            r"a\b",
        ] {
            assert!(
                spellings(hostile).is_empty(),
                "{hostile:?} was turned into a path"
            );
        }
    }

    /// Review MEDIUM 7: D-Bus activation is only honoured for an id that really
    /// is a well-known bus name, because GIO silently forks everything else into
    /// **this shell's own cgroup**.
    ///
    /// **The mutation**: making this return `true` unconditionally reds the
    /// `app_start` row that routes `Alacritty` through the launcher.
    #[test]
    fn only_a_well_formed_bus_name_may_be_activated() {
        for valid in [
            "org.gnome.Nautilus",
            "org.mozilla.firefox",
            "a.b",
            "_x.y-z",
            "com.example.App_1",
        ] {
            assert!(is_valid_bus_name(valid), "{valid:?} should be usable");
        }
        for invalid in [
            "Alacritty",   // one element — the epic's own example id
            "firefox",     // …and the other one
            "",            //
            ".org.gnome",  // empty leading element
            "org.gnome.",  // empty trailing element
            "org..gnome",  // empty interior element
            "org.1gnome",  // element starting with a digit
            "1org.gnome",  //
            ":1.42",       // a unique name, never activatable
            "org.gnome/x", // not a name character
        ] {
            assert!(!is_valid_bus_name(invalid), "{invalid:?} should be refused");
        }
        // The length cap, at the boundary.
        let long = format!("a.{}", "b".repeat(253));
        assert_eq!(long.len(), 255);
        assert!(is_valid_bus_name(&long));
        assert!(!is_valid_bus_name(&format!("{long}b")));
    }

    /// Review LOW 12: GIO refuses to build a `GDesktopAppInfo` whose `TryExec`
    /// is not on `PATH`; this reader goes at the keyfile directly, so it has to
    /// make the same call itself.
    #[test]
    fn try_exec_is_honoured_when_it_names_a_missing_program() {
        let installed = |program: &str| program == "alacritty";
        // No key at all: nothing to check.
        assert!(try_exec_ok(None, installed));
        // Present and installed.
        assert!(try_exec_ok(Some("alacritty"), installed));
        // Present and absent — the case the whole check exists for.
        assert!(!try_exec_ok(Some("not-installed"), installed));
        // Blank is not a claim about anything.
        assert!(try_exec_ok(Some("   "), installed));
        assert!(try_exec_ok(Some(""), installed));
    }

    fn words(line: &str) -> Vec<String> {
        exec_words(line)
    }

    /// The argv a stack app is launched with, end to end: split, then stripped.
    fn resolved(line: &str) -> Vec<String> {
        strip_field_codes(&exec_words(line))
    }

    #[test]
    fn a_plain_exec_line_splits_on_whitespace() {
        assert_eq!(
            words("alacritty -e weechat"),
            ["alacritty", "-e", "weechat"]
        );
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
        assert_eq!(
            words(r#"sh -c 'echo "$@"' x"#),
            ["sh", "-c", r#"echo "$@""#, "x"]
        );
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

    /// An **unknown** code keeps its `%`, so a typo shows up in the failed
    /// command line instead of silently removing an argument.
    #[test]
    fn an_unknown_field_code_is_left_verbatim() {
        assert_eq!(resolved("prog %q %z"), ["prog", "%q", "%z"]);
        assert_eq!(resolved("prog 50%"), ["prog", "50%"]);
    }

    /// …but a **deprecated** code is removed, not kept (review LOW 11).
    ///
    /// The spec says deprecated codes must be removed; leaving them to the
    /// unknown-code arm hands an old entry's program a literal `%d`. Known and
    /// dead is not unknown.
    ///
    /// **The mutation**: dropping the six from `FIELD_CODES` reds this.
    #[test]
    fn every_deprecated_field_code_is_dropped_too() {
        for code in ["%d", "%D", "%n", "%N", "%v", "%m"] {
            assert_eq!(
                resolved(&format!("prog {code}")),
                ["prog"],
                "{code} reached execve as a literal argument"
            );
        }
        assert_eq!(resolved("prog %d %D %n %N %v %m --flag"), ["prog", "--flag"]);
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
