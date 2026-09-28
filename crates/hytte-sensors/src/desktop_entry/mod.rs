//! A desktop-entry resolver for app ids, in plain `std` (#1428).
//!
//! The native Stats page names each Top apps row through
//! `trollshell/src/components/app_meta.rs`'s `resolve_app_meta`: three
//! matching layers over `gio::AppInfo::all()`. `hytte-plugin-stats` cannot
//! link gio, so its rows read the raw app id — `niri-firefox` where native
//! reads `Firefox`. This module is the same lookup without gio: it reads the
//! `.desktop` files itself and runs the same three layers over them, so a
//! plugin gets the name native shows.
//!
//! # Parity with `app_meta.rs`
//!
//! "The same" is checked against the source of the `GLib` the shell links
//! (2.88.3: `gio/gdesktopappinfo.c`, `glib/gkeyfile.c`, `glib/gshell.c`,
//! `glib/gutils.c`, `glib/gcharset.c`), not against the documentation.
//!
//! | | native (`resolve_app_meta` over `AppInfo::all()`) | here |
//! | --- | --- | --- |
//! | **Layer 1** | an entry whose id is `<app_id>.desktop`, or `<lowercased app_id>.desktop`; the comparison itself is case-sensitive | the same two strings, compared the same way |
//! | **Layer 2** | the id without `.desktop`, lowercased, contains the lowercased app id, or is contained in it | the same, with Rust's `to_lowercase` on both sides as native uses |
//! | **Layer 3** | the executable's file stem, lowercased, equals the lowercased app id | the same, over the same executable string (below) |
//! | **Order** | layer 1 over every entry, then layer 2, then layer 3; within a layer the first entry in `all()`'s order | the same layering; "first" is the first in search-path order, then by id (see the gaps) |
//! | **Search path** | `$XDG_DATA_HOME` (default `$HOME/.local/share`), then each `$XDG_DATA_DIRS` entry (default `/usr/local/share/:/usr/share/`), each with `applications/` appended | the same |
//! | **Desktop-file id** | the path under `applications/` with `/` turned into `-`, so `kde/konsole.desktop` is `kde-konsole.desktop` | the same, recursing into subdirectories as `get_apps_from_dir` does |
//! | **Shadowing** | the first directory to hold an id owns it: a same-id file in a later directory is ignored, **even when the first one fails to load or is `Hidden`** — a file masks by existing | the same |
//! | **`Hidden=true`** | not listed (`add_to_table_if_appropriate`), still masks | the same |
//! | **`NoDisplay=true`** | listed — `all()` does not filter on it, only `g_app_info_should_show` does, and `app_meta` never calls that | listed; the key is not read at all |
//! | **Which files are entries** | a regular file whose key file parses, whose first group is `[Desktop Entry]`, `Type=Application`, and whose non-empty `TryExec=` and `Exec=` name a program `g_find_program_for_path` finds on `$PATH` (relative to `Path=` if set) | the same — `keyfile` ports the parser's rules, `exec` the program lookup |
//! | **Display name** | `X-GNOME-FullName`, else `Name`, each localised for `LANGUAGE`/`LC_ALL`/`LC_MESSAGES`/`LANG`, else `Unnamed` | the same, `Unnamed` untranslated |
//! | **Executable** | `binary_from_exec`: `Exec=`'s first space-separated token, verbatim — no quote removal, no `env` skipping, so `Exec=env FOO=1 foot` has the executable `env` | the same |
//! | **Icon** | a `GIcon` built from the localised `Icon=`, with a trailing `.png`/`.svg`/`.xpm` dropped from a theme name | the localised `Icon=` **raw** — nothing reads it yet (#1419's icon question), and the consumer that does decides what to strip |
//! | **Cache** | caller-owned, per app id, a miss cached too — one `all()` scan per unseen id | [`Resolver`] owns it, per app id, a miss cached too — one scan per [`Resolver::resolve_all`] call with any unseen id in it |
//!
//! Localised names are honoured rather than documented away: an
//! `LC_MESSAGES=sv_SE.UTF-8` session gets `Filer` for Nautilus natively, and
//! a plugin row reading `Files` next to it would be exactly the difference
//! this module exists to remove. It is one environment read and a variant
//! list (`g_get_language_names`, [`Env::languages`]).
//!
//! # Known gaps
//!
//! - **The tie-break inside a layer.** Native takes the first match in
//!   `all()`'s order, which is `GLib` hash-table iteration order —
//!   unspecified. Here it is search-path order, then id order. Only an app
//!   id that two entries both match *in the same layer* can come out
//!   differently, and for those native itself has no stable answer.
//! - **`/usr/share/locale/locale.alias`** (`unalias_lang`) is not read, so a
//!   locale alias such as `LANG=swedish` is not expanded. `NixOS` ships no
//!   such file.
//! - **`Unnamed`** is not translated for an entry with no `Name=`.
//! - **`$HOME` unset** falls back to the password database in `GLib`; here
//!   the user data directory is then skipped. An empty `$XDG_DATA_DIRS`
//!   element (`a::b`) is skipped too, where `GLib` would scan a relative
//!   `applications/` under the process's working directory — which differs
//!   between the shell and a plugin anyway.
//! - **Subdirectories deeper than [`MAX_DEPTH`]** are not scanned, where
//!   `GLib` recurses until `readdir` fails. It only matters for a symlink
//!   loop, and there the bound turns an exponential walk into a bounded one.
//! - **A file name that is not UTF-8** is skipped; no app id can equal it.
//! - **An entry with no `Exec=` line** has no executable here, so layer 3
//!   skips it. Natively `AppInfo::executable()` then hands gio-rs a `NULL`
//!   path, which `from_glib_none` only `debug_assert`s against — so on the
//!   native side layer 3 reaching such an entry is a debug-build panic and a
//!   release-build null dereference. That is a latent native bug, not a
//!   behaviour to copy.
//! - **Freshness.** Like native, an app installed after its id was first
//!   looked up keeps the answer that lookup got until the [`Resolver`] is
//!   dropped.

mod exec;
mod keyfile;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use keyfile::{DESKTOP_ENTRY, KeyFile};

/// How deep under an `applications/` directory the scan descends
/// (`applications/a/b.desktop` is depth 1). Real ids nest one level
/// (`kde4/`); see the module docs' gaps for why there is a bound at all.
pub const MAX_DEPTH: usize = 8;

/// What a desktop entry says about an app id: the name to show for it, and
/// its `Icon=` value — the gio-free half of `app_meta.rs`'s `AppMeta`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppMeta {
    /// `X-GNOME-FullName`, else `Name`, localised, else `Unnamed` —
    /// `g_app_info_get_display_name`.
    pub display_name: String,
    /// The localised `Icon=` value exactly as the file spells it: a theme
    /// name or an absolute path. `None` when the entry has none.
    pub icon: Option<String>,
}

/// Everything the resolver takes from the process environment, already
/// turned into plain data — the seam that keeps every test off the real
/// `$XDG_DATA_DIRS`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Env {
    /// Each `applications/` directory to scan, highest precedence first.
    pub dirs: Vec<PathBuf>,
    /// `$PATH`, split: where a `TryExec=`/`Exec=` program has to be for GIO
    /// to list the entry. An empty element is the working directory.
    pub path: Vec<PathBuf>,
    /// The language names a localised key is tried under, most preferred
    /// first, cut where `GLib`'s list reaches `C` (after which only the
    /// untranslated key is read).
    pub languages: Vec<String>,
}

impl Env {
    /// The real process environment — [`Env::from_vars`] over
    /// [`std::env::var_os`].
    #[must_use]
    pub fn from_process() -> Self {
        Self::from_vars(|key| std::env::var_os(key))
    }

    /// The environment `var` describes: `var("XDG_DATA_HOME")` and so on,
    /// resolved the way `GLib` resolves the real one.
    #[must_use]
    pub fn from_vars(var: impl Fn(&str) -> Option<OsString>) -> Self {
        let set = |key: &str| var(key).filter(|value| !value.is_empty());

        let data_home = set("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| set("HOME").map(|home| Path::new(&home).join(".local/share")));
        let data_dirs =
            set("XDG_DATA_DIRS").unwrap_or_else(|| OsString::from("/usr/local/share/:/usr/share/"));
        let dirs = data_home
            .into_iter()
            .chain(std::env::split_paths(&data_dirs).filter(|dir| !dir.as_os_str().is_empty()))
            .map(|dir| dir.join("applications"))
            .collect();

        let path = var("PATH").unwrap_or_else(|| OsString::from("/bin:/usr/bin:."));
        let path = std::env::split_paths(&path).collect();

        Self {
            dirs,
            path,
            languages: language_names(&set),
        }
    }
}

/// `g_get_language_names`: the first of `LANGUAGE`, `LC_ALL`, `LC_MESSAGES`
/// and `LANG` that is set (else `C`), split at `:`, each locale expanded to
/// its variants — then cut at the first `C`, where `GLib`'s localised
/// lookup stops trying translations.
fn language_names(set: &impl Fn(&str) -> Option<OsString>) -> Vec<String> {
    let chosen = ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .find_map(set)
        .map_or_else(
            || "C".to_owned(),
            |value| value.to_string_lossy().into_owned(),
        );
    let mut names: Vec<String> = chosen.split(':').flat_map(locale_variants).collect();
    if let Some(c) = names.iter().position(|name| name == "C") {
        names.truncate(c);
    }
    names
}

/// `append_locale_variants`: `language[_territory][.codeset][@modifier]`
/// and every variant with components dropped, in `GLib`'s own order.
fn locale_variants(locale: &str) -> Vec<String> {
    const CODESET: u8 = 1;
    const TERRITORY: u8 = 2;
    const MODIFIER: u8 = 4;

    let uscore = locale.find('_');
    let dot = locale[uscore.unwrap_or(0)..]
        .find('.')
        .map(|i| i + uscore.unwrap_or(0));
    let at_from = dot.or(uscore).unwrap_or(0);
    let at = locale[at_from..].find('@').map(|i| i + at_from);

    let mut mask = 0;
    let at_pos = at.unwrap_or(locale.len());
    let modifier = at.map_or("", |at| &locale[at..]);
    if at.is_some() {
        mask |= MODIFIER;
    }
    let dot_pos = dot.filter(|&d| d < at_pos).unwrap_or(at_pos);
    let codeset = &locale[dot_pos..at_pos];
    if dot_pos < at_pos {
        mask |= CODESET;
    }
    let uscore_pos = uscore.filter(|&u| u < dot_pos).unwrap_or(dot_pos);
    let territory = &locale[uscore_pos..dot_pos];
    if uscore_pos < dot_pos {
        mask |= TERRITORY;
    }
    let language = &locale[..uscore_pos];

    (0..=mask)
        .rev()
        .filter(|i| i & !mask == 0)
        .map(|i| {
            let pick = |bit: u8, part: &str| {
                if i & bit != 0 {
                    part.to_owned()
                } else {
                    String::new()
                }
            };
            format!(
                "{language}{}{}{}",
                pick(TERRITORY, territory),
                pick(CODESET, codeset),
                pick(MODIFIER, modifier),
            )
        })
        .collect()
}

/// One listed desktop entry — one `GDesktopAppInfo` in `all()`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    /// The desktop-file id, `.desktop` suffix included.
    id: String,
    /// See [`AppMeta::display_name`].
    display_name: String,
    /// See [`AppMeta::icon`].
    icon: Option<String>,
    /// `binary_from_exec` of the `Exec=` line; `None` without one.
    executable: Option<String>,
}

/// Which of the three layers matched — tests assert it, so a case the
/// native lookup catches in layer 2 cannot pass here by way of layer 3.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layer {
    /// `<app_id>.desktop` or its lowercase form.
    Exact,
    /// The id's stem contains the app id, or the app id contains the stem.
    Contains,
    /// The executable's file stem equals the app id.
    Executable,
}

/// `resolve_app_meta`'s three layers over `entries`, in order.
fn find<'e>(app_id: &str, entries: &'e [Entry]) -> Option<(&'e Entry, Layer)> {
    let app_id_lower = app_id.to_lowercase();

    // Layer 1: exact id match.
    let exact = format!("{app_id}.desktop");
    let exact_lower = format!("{app_id_lower}.desktop");
    if let Some(entry) = entries
        .iter()
        .find(|entry| entry.id == exact || entry.id == exact_lower)
    {
        return Some((entry, Layer::Exact));
    }

    // Layer 2: case-insensitive id containment, either way round.
    if let Some(entry) = entries.iter().find(|entry| {
        let stem = entry
            .id
            .strip_suffix(".desktop")
            .unwrap_or(&entry.id)
            .to_lowercase();
        stem.contains(app_id_lower.as_str()) || app_id_lower.contains(stem.as_str())
    }) {
        return Some((entry, Layer::Contains));
    }

    // Layer 3: executable basename match.
    entries
        .iter()
        .find(|entry| {
            entry
                .executable
                .as_deref()
                .and_then(|exe| Path::new(exe).file_stem())
                .and_then(|stem| stem.to_str())
                .is_some_and(|stem| stem.to_lowercase() == app_id_lower)
        })
        .map(|entry| (entry, Layer::Executable))
}

/// Every entry `g_app_info_get_all()` would list for `env`, in search-path
/// order and, within one directory, by id.
fn scan(env: &Env) -> Vec<Entry> {
    // Every id a higher-precedence directory holds a file for, loadable or
    // not — `desktop_file_dir_app_name_is_masked`.
    let mut owned: HashSet<String> = HashSet::new();
    let mut entries = Vec::new();
    for dir in &env.dirs {
        let mut files = BTreeMap::new();
        collect(dir, "", 0, &mut files);
        for (id, path) in files {
            if owned.insert(id.clone())
                && let Some(entry) = load(id, &path, env)
            {
                entries.push(entry);
            }
        }
    }
    entries
}

/// `get_apps_from_dir`: every `*.desktop` name under `dir`, keyed by its
/// desktop-file id (`prefix` carries the subdirectories, each ending in `-`).
fn collect(dir: &Path, prefix: &str, depth: usize, files: &mut BTreeMap<String, PathBuf>) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return;
    };
    let mut names: Vec<OsString> = read.filter_map(Result::ok).map(|e| e.file_name()).collect();
    names.sort();
    for name in names {
        let Some(name) = name.to_str() else {
            continue;
        };
        let path = dir.join(name);
        if name.ends_with(".desktop") {
            files.entry(format!("{prefix}{name}")).or_insert(path);
        } else if depth < MAX_DEPTH && path.is_dir() {
            collect(&path, &format!("{prefix}{name}-"), depth + 1, files);
        }
    }
}

/// `g_desktop_app_info_new_from_filename` plus `add_to_table_if_appropriate`:
/// the entry at `path`, or `None` where GIO lists nothing for it.
fn load(id: String, path: &Path, env: &Env) -> Option<Entry> {
    // `g_key_file_load_from_fd`: "Not a regular file".
    if !std::fs::metadata(path).is_ok_and(|meta| meta.is_file()) {
        return None;
    }
    let file = KeyFile::parse(&std::fs::read(path).ok()?)?;
    if file.start_group() != Some(DESKTOP_ENTRY)
        || file.string("Type").ok().flatten().as_deref() != Some("Application")
    {
        return None;
    }
    // A present-but-unreadable `Path=`, `TryExec=` or `Exec=` drops the
    // entry (`is_invalid_key_error`); an absent one does not.
    let working_dir = file.string("Path").ok()?;
    let working_dir = working_dir.as_deref();
    if let Some(try_exec) = file.string("TryExec").ok()?.filter(|t| !t.is_empty())
        && !exec::find_program(&try_exec, &env.path, working_dir)
    {
        return None;
    }
    let exec_line = file.string("Exec").ok()?;
    if let Some(line) = exec_line.as_deref().filter(|line| !line.is_empty())
        && !exec::find_program(&exec::first_word(line)?, &env.path, working_dir)
    {
        return None;
    }
    if file.boolean("Hidden") {
        return None;
    }
    let display_name = file
        .locale_string("X-GNOME-FullName", &env.languages)
        .or_else(|| file.locale_string("Name", &env.languages))
        .unwrap_or_else(|| "Unnamed".to_owned());
    Some(Entry {
        id,
        display_name,
        icon: file.locale_string("Icon", &env.languages),
        executable: exec_line
            .as_deref()
            .map(|line| exec::binary(line).to_owned()),
    })
}

/// App id → [`AppMeta`], cached — the gio-free `resolve_app_meta` plus the
/// `MetaCache` its callers own.
///
/// A lookup that misses the cache scans the whole search path once; the
/// answer, a miss included, is kept for as long as the resolver lives. Scans
/// read files, so a caller on an async runtime resolves off it (the stats
/// plugin resolves inside the walk it already runs under `spawn_blocking`).
#[derive(Debug)]
pub struct Resolver {
    env: Env,
    cache: HashMap<String, Option<AppMeta>>,
    scans: usize,
}

impl Resolver {
    /// A resolver over `env`, with nothing cached.
    #[must_use]
    pub fn new(env: Env) -> Self {
        Self {
            env,
            cache: HashMap::new(),
            scans: 0,
        }
    }

    /// A resolver over the real process environment ([`Env::from_process`]).
    /// Reads environment variables only; the first scan waits for the first
    /// lookup.
    #[must_use]
    pub fn from_env() -> Self {
        Self::new(Env::from_process())
    }

    /// The entry `app_id` resolves to, or `None` when no desktop entry
    /// matches. Scans on a cache miss.
    pub fn resolve(&mut self, app_id: &str) -> Option<&AppMeta> {
        self.resolve_all([app_id]);
        self.cache.get(app_id).and_then(Option::as_ref)
    }

    /// Resolve every id in `app_ids` into the cache, with **at most one
    /// scan** for all the ones not cached yet — so a walk that meets six new
    /// apps reads the search path once, not six times. [`resolve`] then
    /// answers each of them from the cache.
    ///
    /// [`resolve`]: Self::resolve
    pub fn resolve_all<'a>(&mut self, app_ids: impl IntoIterator<Item = &'a str>) {
        let misses: Vec<&str> = app_ids
            .into_iter()
            .filter(|id| !self.cache.contains_key(*id))
            .collect();
        if misses.is_empty() {
            return;
        }
        let entries = scan(&self.env);
        self.scans += 1;
        for app_id in misses {
            let meta = find(app_id, &entries).map(|(entry, _)| AppMeta {
                display_name: entry.display_name.clone(),
                icon: entry.icon.clone(),
            });
            self.cache.insert(app_id.to_owned(), meta);
        }
    }

    /// How many times this resolver has read the search path.
    #[must_use]
    pub fn scans(&self) -> usize {
        self.scans
    }
}

#[cfg(test)]
mod tests;
