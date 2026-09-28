//! Every test here reads fixture `.desktop` files in a tempdir through an
//! [`Env`] it builds itself, never the host's `$XDG_DATA_DIRS` — except the
//! one re-exec test at the bottom, which pins [`Env::from_process`] in a
//! child process whose environment points at fixtures too.

use std::collections::HashMap;
use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use super::{AppMeta, Env, Layer, Resolver, find, locale_variants, scan};

/// A fixture search path: data directories under one tempdir, and a `bin/`
/// that is the whole `$PATH`, holding the programs the entries' `Exec=`
/// lines name.
struct Fixture {
    root: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let fixture = Self {
            root: tempfile::tempdir().expect("tempdir"),
        };
        for program in [
            "firefox",
            "nautilus",
            "foot",
            "Foot",
            "env",
            "konsole",
            "alacritty",
        ] {
            fixture.program(program);
        }
        fixture
    }

    fn bin(&self) -> PathBuf {
        self.root.path().join("bin")
    }

    /// An executable file at `bin/<name>`.
    fn program(&self, name: &str) -> PathBuf {
        let path = self.bin().join(name);
        std::fs::create_dir_all(self.bin()).expect("mkdir bin");
        std::fs::write(&path, "#!/bin/sh\n").expect("write program");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    /// Write `body` to `<data>/applications/<rel>`.
    fn entry(&self, data: &str, rel: &str, body: &str) {
        let path = self.root.path().join(data).join("applications").join(rel);
        std::fs::create_dir_all(path.parent().expect("has a parent")).expect("mkdir");
        std::fs::write(path, body).expect("write entry");
    }

    /// The search path `data` (highest precedence first) with `bin/` as
    /// `$PATH` and no language preference.
    fn env(&self, data: &[&str]) -> Env {
        Env {
            dirs: data
                .iter()
                .map(|d| self.root.path().join(d).join("applications"))
                .collect(),
            path: vec![self.bin()],
            languages: Vec::new(),
        }
    }
}

/// A minimal valid application entry.
fn app(name: &str, exec: &str) -> String {
    format!("[Desktop Entry]\nType=Application\nName={name}\nExec={exec}\n")
}

/// Resolve `app_id` over `env` and say which layer matched.
fn lookup(env: &Env, app_id: &str) -> Option<(String, Layer)> {
    let entries = scan(env);
    find(app_id, &entries).map(|(entry, layer)| (entry.display_name.clone(), layer))
}

fn hit(name: &str, layer: Layer) -> (String, Layer) {
    (name.to_owned(), layer)
}

/// **Layer 1**, both spellings: `<app_id>.desktop` compared case-sensitively,
/// then `<lowercased app_id>.desktop`. The reverse-DNS id a Flatpak scope
/// carries is caught here, exactly.
#[test]
fn layer_1_matches_the_id_or_its_lowercase_form() {
    let f = Fixture::new();
    f.entry(
        "share",
        "org.gnome.Nautilus.desktop",
        &app("Files", "nautilus --new-window"),
    );
    f.entry("share", "alacritty.desktop", &app("Alacritty", "alacritty"));
    let env = f.env(&["share"]);

    assert_eq!(
        lookup(&env, "org.gnome.Nautilus"),
        Some(hit("Files", Layer::Exact))
    );
    assert_eq!(
        lookup(&env, "Alacritty"),
        Some(hit("Alacritty", Layer::Exact)),
        "the lowercase spelling is layer 1 too",
    );
}

/// **Layer 2**, both directions, and case-insensitive: the stem contains the
/// app id (a scope that says `nautilus` for `org.gnome.Nautilus.desktop`),
/// or the app id contains the stem (a `NixOS` wrapper `firefox-unwrapped`,
/// and niri's `niri-firefox` — #1428's own case).
#[test]
fn layer_2_matches_containment_either_way() {
    let f = Fixture::new();
    f.entry(
        "share",
        "org.gnome.Nautilus.desktop",
        &app("Files", "nautilus"),
    );
    f.entry("share", "firefox.desktop", &app("Firefox", "firefox %u"));
    let env = f.env(&["share"]);

    assert_eq!(
        lookup(&env, "nautilus"),
        Some(hit("Files", Layer::Contains))
    );
    assert_eq!(
        lookup(&env, "firefox-unwrapped"),
        Some(hit("Firefox", Layer::Contains))
    );
    assert_eq!(
        lookup(&env, "niri-firefox"),
        Some(hit("Firefox", Layer::Contains)),
        "niri's spawn scope: the id contains the entry's stem",
    );
}

/// `app_meta.rs`'s doc credits **layer 3** with the cgroup-scope `firefox` →
/// `Firefox.desktop` case. Natively it never gets there: layer 1 misses
/// (`Firefox.desktop` is neither `firefox.desktop` spelling) but layer 2's
/// lowercased stem `firefox` contains `firefox`. This port agrees with the
/// code, not the comment.
#[test]
fn a_capitalised_entry_for_a_lowercase_scope_is_layer_2_not_3() {
    let f = Fixture::new();
    f.entry("share", "Firefox.desktop", &app("Firefox", "firefox"));
    assert_eq!(
        lookup(&f.env(&["share"]), "firefox"),
        Some(hit("Firefox", Layer::Contains))
    );
}

/// **Layer 3**: nothing in the id relates to the app id, but the
/// executable's file stem does — case-insensitively, and with the stem's
/// extension dropped as `Path::file_stem` drops it.
#[test]
fn layer_3_matches_the_executables_file_stem() {
    let f = Fixture::new();
    f.entry("share", "web-browser.desktop", &app("Web", "firefox %u"));
    let foot = f.bin().join("Foot");
    f.entry(
        "share",
        "terminal.desktop",
        &app("Terminal", &foot.display().to_string()),
    );
    let env = f.env(&["share"]);
    assert_eq!(lookup(&env, "firefox"), Some(hit("Web", Layer::Executable)));
    assert_eq!(
        lookup(&env, "foot"),
        Some(hit("Terminal", Layer::Executable)),
        "case-insensitive"
    );

    let script = f.program("firefox.sh");
    f.entry(
        "scripts",
        "launcher.desktop",
        &app("Launcher", &format!("{} --x", script.display())),
    );
    assert_eq!(
        lookup(&f.env(&["scripts"]), "firefox"),
        Some(hit("Launcher", Layer::Executable)),
        "`firefox.sh`'s file stem is `firefox`",
    );
}

/// The executable is GIO's `binary_from_exec`, so an `env FOO=1 foot` entry's
/// executable is `env`: layer 3 does **not** find it for `foot`, exactly as
/// natively, and does find it for `env`.
#[test]
fn layer_3_does_not_look_past_an_env_prefix() {
    let f = Fixture::new();
    f.entry(
        "share",
        "wrapped.desktop",
        &app("Wrapped", "env FOO=1 foot"),
    );
    let env = f.env(&["share"]);
    assert_eq!(lookup(&env, "foot"), None);
    assert_eq!(lookup(&env, "env"), Some(hit("Wrapped", Layer::Executable)));
}

/// **The layers run in order over every entry**: an entry that matches a
/// later layer and scans first never beats one that matches an earlier layer.
#[test]
fn an_earlier_layer_wins_over_an_earlier_entry() {
    let f = Fixture::new();
    // Scans before `firefox.desktop` (ids sort) and matches layer 2.
    f.entry(
        "share",
        "aaa-firefox-beta.desktop",
        &app("Firefox Beta", "firefox"),
    );
    f.entry("share", "firefox.desktop", &app("Firefox", "firefox"));
    // Scans before `zzz-foot.desktop` and matches layer 3 only.
    f.entry("share", "a-terminal.desktop", &app("A Terminal", "foot"));
    f.entry(
        "share",
        "zzz-foot.desktop",
        &app("Foot Server", "foot --server"),
    );
    let env = f.env(&["share"]);

    assert_eq!(lookup(&env, "firefox"), Some(hit("Firefox", Layer::Exact)));
    assert_eq!(
        lookup(&env, "foot"),
        Some(hit("Foot Server", Layer::Contains))
    );
}

/// **Precedence**: the first directory that holds an id owns it, so the
/// user's entry shadows the system's and an earlier `$XDG_DATA_DIRS` entry
/// shadows a later one.
#[test]
fn an_earlier_directory_shadows_a_later_one() {
    let f = Fixture::new();
    f.entry("home", "firefox.desktop", &app("Mine", "firefox"));
    f.entry("sys-a", "firefox.desktop", &app("Theirs", "firefox"));
    f.entry("sys-a", "foot.desktop", &app("Foot A", "foot"));
    f.entry("sys-b", "foot.desktop", &app("Foot B", "foot"));

    let env = f.env(&["home", "sys-a", "sys-b"]);
    assert_eq!(lookup(&env, "firefox"), Some(hit("Mine", Layer::Exact)));
    assert_eq!(lookup(&env, "foot"), Some(hit("Foot A", Layer::Exact)));
    let ids: Vec<String> = scan(&env).into_iter().map(|e| e.id).collect();
    assert_eq!(ids, ["firefox.desktop", "foot.desktop"], "one entry per id");

    let reversed = f.env(&["sys-b", "sys-a", "home"]);
    assert_eq!(lookup(&reversed, "foot"), Some(hit("Foot B", Layer::Exact)));
}

/// **`Hidden=true` deletes the id**: the hidden entry is not listed, and it
/// still masks the same id further down the search path — a user's way to
/// remove a system entry.
#[test]
fn a_hidden_entry_is_unlisted_and_still_masks() {
    let f = Fixture::new();
    f.entry(
        "home",
        "firefox.desktop",
        "[Desktop Entry]\nType=Application\nName=X\nHidden=true\n",
    );
    f.entry("share", "firefox.desktop", &app("Firefox", "firefox"));
    assert_eq!(lookup(&f.env(&["home", "share"]), "firefox"), None);
    assert_eq!(
        lookup(&f.env(&["share"]), "firefox"),
        Some(hit("Firefox", Layer::Exact)),
        "the control: without the mask the system entry resolves",
    );
}

/// A file that **fails to load masks too** — GIO masks by file name, before
/// it tries to load anything.
#[test]
fn an_unloadable_file_still_masks() {
    let f = Fixture::new();
    f.entry(
        "home",
        "firefox.desktop",
        "[Desktop Entry]\nnot a key file line\n",
    );
    f.entry("share", "firefox.desktop", &app("Firefox", "firefox"));
    assert_eq!(lookup(&f.env(&["home", "share"]), "firefox"), None);
}

/// `NoDisplay=true` entries are listed — `all()` does not filter them.
#[test]
fn a_nodisplay_entry_still_resolves() {
    let f = Fixture::new();
    f.entry(
        "share",
        "foot.desktop",
        &format!("{}NoDisplay=true\n", app("Foot", "foot")),
    );
    assert_eq!(
        lookup(&f.env(&["share"]), "foot"),
        Some(hit("Foot", Layer::Exact))
    );
}

/// Every file GIO refuses to list, each of which would otherwise have
/// matched `firefox` in layer 1.
#[test]
fn the_files_gio_does_not_list_never_match() {
    for (why, body) in [
        (
            "no Type",
            "[Desktop Entry]\nName=F\nExec=firefox\n".to_owned(),
        ),
        (
            "a Link",
            "[Desktop Entry]\nType=Link\nName=F\nURL=x\n".to_owned(),
        ),
        (
            "another start group",
            format!("[Other]\nA=1\n{}", app("F", "firefox")),
        ),
        (
            "a malformed line",
            format!("{}garbage\n", app("F", "firefox")),
        ),
        ("Exec off PATH", app("F", "ghost")),
        (
            "an absolute Exec that is missing",
            app("F", "/nonexistent/firefox"),
        ),
        ("an unbalanced Exec", app("F", "'firefox")),
        ("a whitespace-only Exec", app("F", "\\s\\s")),
        ("an Exec with a bad escape", app("F", "fire\\qfox")),
        (
            "TryExec off PATH",
            format!("{}TryExec=ghost\n", app("F", "firefox")),
        ),
    ] {
        let f = Fixture::new();
        f.entry("share", "firefox.desktop", &body);
        assert_eq!(lookup(&f.env(&["share"]), "firefox"), None, "{why}");
    }
}

/// A file GIO does list although it looks incomplete: no `Exec=` at all, an
/// empty `Exec=`, a `TryExec=` that is found, an `Exec=` found relative to
/// `Path=`.
#[test]
fn the_files_gio_lists_despite_looking_thin() {
    let f = Fixture::new();
    f.entry(
        "share",
        "a.desktop",
        "[Desktop Entry]\nType=Application\nName=No Exec\n",
    );
    f.entry(
        "share",
        "b.desktop",
        "[Desktop Entry]\nType=Application\nName=Empty\nExec=\n",
    );
    f.entry(
        "share",
        "c.desktop",
        &format!("{}TryExec=foot\n", app("Tried", "foot")),
    );
    let root = f.root.path().display().to_string();
    f.entry(
        "share",
        "d.desktop",
        &format!("{}Path={root}\n", app("Relative", "bin/konsole")),
    );
    let env = f.env(&["share"]);
    let names: Vec<String> = scan(&env).into_iter().map(|e| e.display_name).collect();
    assert_eq!(names, ["No Exec", "Empty", "Tried", "Relative"]);
}

/// **Desktop-file ids** follow the XDG rule `get_apps_from_dir` implements:
/// `applications/kde/konsole.desktop` is `kde-konsole.desktop`, and it is
/// masked by a `kde-konsole.desktop` in an earlier directory.
#[test]
fn a_subdirectory_becomes_part_of_the_id() {
    let f = Fixture::new();
    f.entry("share", "kde/konsole.desktop", &app("Konsole", "konsole"));
    let env = f.env(&["share"]);
    let ids: Vec<String> = scan(&env).into_iter().map(|e| e.id).collect();
    assert_eq!(ids, ["kde-konsole.desktop"]);
    assert_eq!(
        lookup(&env, "kde-konsole"),
        Some(hit("Konsole", Layer::Exact))
    );
    assert_eq!(
        lookup(&env, "konsole"),
        Some(hit("Konsole", Layer::Contains))
    );

    f.entry("home", "kde-konsole.desktop", &app("Mine", "konsole"));
    assert_eq!(
        lookup(&f.env(&["home", "share"]), "kde-konsole"),
        Some(hit("Mine", Layer::Exact))
    );
}

/// The display name is `g_app_info_get_display_name`'s: a localised
/// `X-GNOME-FullName`, else a localised `Name`, else `Unnamed`; the icon is
/// the localised `Icon=` as written.
#[test]
fn the_display_name_and_icon_are_gios() {
    let f = Fixture::new();
    f.entry(
        "share",
        "org.gnome.Nautilus.desktop",
        "[Desktop Entry]\nType=Application\nName=Files\nName[sv]=Filer\nIcon=org.gnome.Nautilus\nExec=nautilus\n",
    );
    f.entry(
        "share",
        "gimp.desktop",
        "[Desktop Entry]\nType=Application\nName=GIMP\nX-GNOME-FullName=GNU Image Manipulation Program\nIcon=/opt/gimp.png\n",
    );
    f.entry(
        "share",
        "nameless.desktop",
        "[Desktop Entry]\nType=Application\n",
    );
    let mut env = f.env(&["share"]);
    let mut resolver = Resolver::new(env.clone());
    assert_eq!(
        resolver.resolve("nautilus").cloned(),
        Some(AppMeta {
            display_name: "Files".to_owned(),
            icon: Some("org.gnome.Nautilus".to_owned()),
        }),
    );
    assert_eq!(
        resolver.resolve("gimp").cloned(),
        Some(AppMeta {
            display_name: "GNU Image Manipulation Program".to_owned(),
            icon: Some("/opt/gimp.png".to_owned()),
        }),
    );
    assert_eq!(
        resolver
            .resolve("nameless")
            .map(|m| m.display_name.as_str()),
        Some("Unnamed")
    );

    env.languages = vec!["sv_SE".to_owned(), "sv".to_owned()];
    assert_eq!(
        lookup(&env, "nautilus"),
        Some(hit("Filer", Layer::Contains))
    );
}

/// **The cache**: a hit is not re-read (the file can go and the answer
/// stays), a miss is not re-read either (a file added later is not seen),
/// and only a fresh resolver scans again.
#[test]
fn the_cache_answers_without_rescanning() {
    let f = Fixture::new();
    f.entry("share", "firefox.desktop", &app("Firefox", "firefox"));
    let env = f.env(&["share"]);
    let mut resolver = Resolver::new(env.clone());
    assert_eq!(resolver.scans(), 0, "construction reads nothing");

    let name = |r: &mut Resolver, id: &str| r.resolve(id).map(|m| m.display_name.clone());
    assert_eq!(
        name(&mut resolver, "niri-firefox").as_deref(),
        Some("Firefox")
    );
    assert_eq!(name(&mut resolver, "ghost"), None);
    assert_eq!(resolver.scans(), 2, "one scan per unseen id");

    std::fs::remove_file(env.dirs[0].join("firefox.desktop")).expect("rm");
    f.entry("share", "ghost.desktop", &app("Ghost", "foot"));
    assert_eq!(
        name(&mut resolver, "niri-firefox").as_deref(),
        Some("Firefox"),
        "a cached hit"
    );
    assert_eq!(name(&mut resolver, "ghost"), None, "a cached miss");
    assert_eq!(resolver.scans(), 2, "neither lookup scanned");

    let mut fresh = Resolver::new(env);
    assert_eq!(
        name(&mut fresh, "ghost").as_deref(),
        Some("Ghost"),
        "the control: the file is there"
    );
    assert_eq!(name(&mut fresh, "niri-firefox"), None);
}

/// **One scan per batch**: `resolve_all` over several unseen ids reads the
/// search path once, and a batch of ids already cached reads nothing.
#[test]
fn resolve_all_scans_once_for_every_miss_in_it() {
    let f = Fixture::new();
    f.entry("share", "firefox.desktop", &app("Firefox", "firefox"));
    f.entry("share", "foot.desktop", &app("Foot", "foot"));
    let mut resolver = Resolver::new(f.env(&["share"]));

    resolver.resolve_all(["niri-firefox", "niri-foot", "ghost", "niri-firefox"]);
    assert_eq!(resolver.scans(), 1);
    assert_eq!(
        resolver
            .resolve("niri-foot")
            .map(|m| m.display_name.as_str()),
        Some("Foot")
    );
    resolver.resolve_all(["niri-firefox", "ghost"]);
    resolver.resolve_all([]);
    assert_eq!(resolver.scans(), 1, "everything was cached");
}

/// A resolver over no directories resolves nothing and does not fail.
#[test]
fn an_empty_search_path_resolves_nothing() {
    let mut resolver = Resolver::new(Env::default());
    assert_eq!(resolver.resolve("firefox"), None);
}

fn vars(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
    let map: HashMap<String, OsString> = pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), OsString::from(v)))
        .collect();
    move |key| map.get(key).cloned()
}

/// The search path is `GLib`'s: the user data directory first
/// (`$XDG_DATA_HOME`, else `$HOME/.local/share`), then `$XDG_DATA_DIRS`
/// (default `/usr/local/share/:/usr/share/`), each with `applications`.
#[test]
fn the_search_path_is_glibs() {
    let dirs = |pairs: &[(&str, &str)]| Env::from_vars(vars(pairs)).dirs;
    assert_eq!(
        dirs(&[("HOME", "/h")]),
        [
            PathBuf::from("/h/.local/share/applications"),
            PathBuf::from("/usr/local/share/applications"),
            PathBuf::from("/usr/share/applications"),
        ],
    );
    assert_eq!(
        dirs(&[
            ("HOME", "/h"),
            ("XDG_DATA_HOME", "/d"),
            ("XDG_DATA_DIRS", "/a::/b/")
        ]),
        [
            PathBuf::from("/d/applications"),
            PathBuf::from("/a/applications"),
            PathBuf::from("/b/applications"),
        ],
        "XDG_DATA_HOME wins over HOME; an empty XDG_DATA_DIRS element is skipped",
    );
    assert_eq!(
        dirs(&[("HOME", "/h"), ("XDG_DATA_HOME", ""), ("XDG_DATA_DIRS", "")])[0],
        PathBuf::from("/h/.local/share/applications"),
        "an empty variable counts as unset",
    );
    assert_eq!(
        dirs(&[("XDG_DATA_DIRS", "/a")]),
        [PathBuf::from("/a/applications")],
        "no HOME"
    );
}

/// `$PATH` split as `g_find_program_for_path` walks it, with its own
/// default when unset.
#[test]
fn the_program_path_is_glibs() {
    let path = |pairs: &[(&str, &str)]| Env::from_vars(vars(pairs)).path;
    assert_eq!(
        path(&[("PATH", "/x:/y")]),
        [PathBuf::from("/x"), PathBuf::from("/y")]
    );
    assert_eq!(
        path(&[]),
        [
            PathBuf::from("/bin"),
            PathBuf::from("/usr/bin"),
            PathBuf::from(".")
        ],
    );
}

/// `g_get_language_names`: the first of `LANGUAGE`, `LC_ALL`, `LC_MESSAGES`,
/// `LANG` that is set, split at `:`, each expanded; cut at `C`.
#[test]
fn the_language_names_are_glibs() {
    let langs = |pairs: &[(&str, &str)]| Env::from_vars(vars(pairs)).languages;
    assert_eq!(
        langs(&[("LANG", "sv_SE.UTF-8")]),
        ["sv_SE.UTF-8", "sv_SE", "sv.UTF-8", "sv"]
    );
    assert_eq!(
        langs(&[("LANG", "sv_SE"), ("LC_MESSAGES", "de_DE")]),
        ["de_DE", "de"]
    );
    assert_eq!(langs(&[("LC_MESSAGES", "de_DE"), ("LC_ALL", "fr")]), ["fr"]);
    assert_eq!(
        langs(&[("LC_ALL", "fr"), ("LANGUAGE", "nb:sv")]),
        ["nb", "sv"]
    );
    assert_eq!(
        langs(&[("LANGUAGE", ""), ("LANG", "sv")]),
        ["sv"],
        "empty LANGUAGE is unset"
    );
    assert!(langs(&[]).is_empty(), "unset is C: no translations");
    assert!(langs(&[("LANG", "C")]).is_empty());
    assert_eq!(
        langs(&[("LANG", "C.UTF-8")]),
        ["C.UTF-8"],
        "tried, then cut at its own C variant"
    );
    assert_eq!(langs(&[("LANGUAGE", "sv:C:de")]), ["sv"], "C cuts the list");
}

/// `append_locale_variants`, in `GLib`'s own order (a modifier outranks a
/// codeset, which is not what `g_get_locale_variants`' doc example says —
/// the code is what the shell runs).
#[test]
fn locale_variants_are_glibs() {
    assert_eq!(locale_variants("fr_BE"), ["fr_BE", "fr"]);
    assert_eq!(
        locale_variants("en_GB.UTF-8@euro"),
        [
            "en_GB.UTF-8@euro",
            "en_GB@euro",
            "en.UTF-8@euro",
            "en@euro",
            "en_GB.UTF-8",
            "en_GB",
            "en.UTF-8",
            "en",
        ],
    );
    assert_eq!(locale_variants("sr@latin"), ["sr@latin", "sr"]);
    assert_eq!(locale_variants("C"), ["C"]);
}

/// Marks the re-exec'd child of [`from_process_reads_the_real_environment`]
/// and carries its fixture root; unset in an ordinary run, where the inner
/// test returns at once.
const CHILD_FIXTURE: &str = "HYTTE_SENSORS_DESKTOP_ENTRY_TEST_CHILD";

/// Printed by the child only after every assertion passed, so a stale
/// `--exact` filter that runs nothing cannot pass for a success.
const CHILD_OK: &str = "desktop-entry-env-child-reached-the-end";

/// **`Env::from_process` reads the real process environment** — pinned in
/// a re-exec'd child, because `std::env::set_var` is `unsafe` in edition 2024
/// and this workspace forbids `unsafe`. The child's `XDG_DATA_HOME`,
/// `XDG_DATA_DIRS`, `HOME`, `PATH` and locale variables all point at
/// fixtures, so it reads none of the host's entries either.
///
/// **Falsified** by `from_process` passing `|_| None` to `from_vars`: the
/// child then sees the built-in defaults and its first assertion reds.
#[test]
fn from_process_reads_the_real_environment() {
    let f = Fixture::new();
    f.entry("sys-a", "firefox.desktop", "[Desktop Entry]\nType=Application\nName=Firefox\nName[sv]=Firefox på svenska\nExec=firefox\n");
    let root = f.root.path();
    let inner = "desktop_entry::tests::from_process_reads_the_real_environment_inner";
    let out = std::process::Command::new(std::env::current_exe().expect("this test binary"))
        .args(["--exact", "--nocapture", "--test-threads=1", inner])
        .env(CHILD_FIXTURE, root)
        .env("XDG_DATA_HOME", root.join("home-data"))
        .env(
            "XDG_DATA_DIRS",
            format!(
                "{}:{}",
                root.join("sys-a").display(),
                root.join("sys-b").display()
            ),
        )
        .env("HOME", root)
        .env("PATH", f.bin())
        .env_remove("LANGUAGE")
        .env_remove("LC_ALL")
        .env_remove("LC_MESSAGES")
        .env("LANG", "sv_SE.UTF-8")
        .output()
        .expect("re-exec this test binary");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "the child failed\n{stdout}\n{stderr}");
    assert!(
        stdout.contains(CHILD_OK),
        "the child ran nothing\n{stdout}\n{stderr}"
    );
}

/// The body of [`from_process_reads_the_real_environment`]; a no-op
/// outside its child.
#[test]
fn from_process_reads_the_real_environment_inner() {
    let Some(root) = std::env::var_os(CHILD_FIXTURE).map(PathBuf::from) else {
        return;
    };
    assert_eq!(
        Env::from_process(),
        Env {
            dirs: vec![
                root.join("home-data/applications"),
                root.join("sys-a/applications"),
                root.join("sys-b/applications"),
            ],
            path: vec![root.join("bin")],
            languages: ["sv_SE.UTF-8", "sv_SE", "sv.UTF-8", "sv"]
                .map(str::to_owned)
                .to_vec(),
        },
    );
    let mut resolver = Resolver::from_env();
    assert_eq!(
        resolver
            .resolve("niri-firefox")
            .map(|m| m.display_name.as_str()),
        Some("Firefox på svenska"),
        "from_env resolves over the environment's own search path",
    );
    println!("{CHILD_OK}");
}

// ── From the #1431 adversarial review (comment 5877189602), verbatim ──────────

/// A symlink loop under `applications/` stops at [`super::MAX_DEPTH`]:
/// `foot.desktop` once per level, down to the bound and no further. Without
/// the bound the walk runs until the kernel answers `ELOOP` (40 levels).
#[test]
fn a_symlink_loop_is_walked_to_the_bound_and_no_further() {
    let f = Fixture::new();
    f.entry("share", "foot.desktop", &app("Foot", "foot"));
    let apps = f.root.path().join("share/applications");
    std::os::unix::fs::symlink(".", apps.join("loop")).expect("symlink");
    let ids: Vec<String> = scan(&f.env(&["share"])).into_iter().map(|e| e.id).collect();
    assert_eq!(ids.len(), super::MAX_DEPTH + 1, "{ids:?}");
}

/// `GLib` recurses without a bound, so an entry two levels down is listed
/// (`a/b/konsole.desktop` is `a-b-konsole.desktop`).
#[test]
fn a_nested_subdirectory_is_scanned_too() {
    let f = Fixture::new();
    f.entry("share", "a/b/konsole.desktop", &app("Konsole", "konsole"));
    let ids: Vec<String> = scan(&f.env(&["share"])).into_iter().map(|e| e.id).collect();
    assert_eq!(ids, ["a-b-konsole.desktop"]);
}

/// GIO checks `Exec=`'s first word **after** `g_shell_parse_argv` unquotes
/// it, so `Exec="firefox" %u` is listed; layer 3 still compares
/// `binary_from_exec`'s raw token, quote and all, so it does not match
/// `firefox` there, exactly as natively.
#[test]
fn a_quoted_exec_is_listed_by_its_unquoted_program() {
    let f = Fixture::new();
    f.entry("share", "web.desktop", &app("Web", "\"firefox\" %u"));
    let env = f.env(&["share"]);
    let names: Vec<String> = scan(&env).into_iter().map(|e| e.display_name).collect();
    assert_eq!(names, ["Web"], "the PATH check sees the unquoted program");
    assert_eq!(lookup(&env, "firefox"), None, "layer 3 sees the quote");
}

/// An empty `TryExec=` is skipped (`try_exec[0] != '\0'`), and an unreadable
/// `Path=` drops the entry (`is_invalid_key_error`).
#[test]
fn an_empty_tryexec_lists_and_an_unreadable_path_does_not() {
    let f = Fixture::new();
    f.entry(
        "share",
        "firefox.desktop",
        &format!("{}TryExec=\n", app("Firefox", "firefox")),
    );
    f.entry(
        "share",
        "foot.desktop",
        &format!("{}Path=bad\\q\n", app("Foot", "foot")),
    );
    let names: Vec<String> = scan(&f.env(&["share"]))
        .into_iter()
        .map(|e| e.display_name)
        .collect();
    assert_eq!(names, ["Firefox"]);
}

/// A FIFO named `*.desktop` masks like any file but is never opened: an
/// `open(O_RDONLY)` on it blocks until a writer appears, and this scan runs
/// inside the walker's `spawn_blocking`.
#[test]
fn a_fifo_masks_without_being_opened() {
    let f = Fixture::new();
    let home = f.root.path().join("home/applications");
    std::fs::create_dir_all(&home).expect("mkdir");
    nix::unistd::mkfifo(&home.join("firefox.desktop"), nix::sys::stat::Mode::S_IRWXU)
        .expect("mkfifo");
    f.entry("share", "firefox.desktop", &app("Firefox", "firefox"));
    let env = f.env(&["home", "share"]);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || tx.send(scan(&env).len()));
    assert_eq!(
        rx.recv_timeout(std::time::Duration::from_secs(10)),
        Ok(0),
        "the FIFO masks the system entry and the scan returns",
    );
}

/// The cache is keyed by the id as asked: a Flatpak scope's mixed-case
/// `org.gnome.Nautilus` is answered from the cache on the next walk, not
/// missed and rescanned every 2 s.
#[test]
fn a_mixed_case_id_is_cached_under_its_own_spelling() {
    let f = Fixture::new();
    f.entry(
        "share",
        "org.gnome.Nautilus.desktop",
        &app("Files", "nautilus"),
    );
    let mut resolver = Resolver::new(f.env(&["share"]));
    for _ in 0..2 {
        assert_eq!(
            resolver
                .resolve("org.gnome.Nautilus")
                .map(|m| m.display_name.as_str()),
            Some("Files"),
        );
    }
    assert_eq!(resolver.scans(), 1);
}
