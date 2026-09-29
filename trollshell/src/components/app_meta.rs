//! `app_meta` — resolve a Wayland/`cgroup` app-id to a desktop entry's display
//! name and icon.
//!
//! Extracted from `panels/stats.rs` (where it served the "Top apps" expanders
//! alone) when the Workspaces page (#1071 phase 1) became a second consumer: a
//! workspace card's stack row is a strip of app icons resolved from the
//! `app_id` niri reports for each window on that workspace. Two panels
//! resolving app-ids through two copies of these heuristics is exactly the bug
//! farm `components/` exists to prevent — same reasoning as `reactive_list`
//! (#174) and `hytte-config` one layer up (#640).
//!
//! # One copy of the lookup (#1432)
//!
//! Which desktop entry an app id means is decided in
//! [`hytte_sensors::desktop_entry`], not here. That module is a plain-`std`
//! port of GIO's desktop-entry rules (#1428), written so `hytte-plugin-stats`
//! — which cannot link gio — names its Top apps rows the way the native Stats
//! page does. Until #1432 this file carried its own copy of the same three
//! layers over `gio::AppInfo::all()`, and nothing checked the two copies
//! agreed. Now the shell asks the resolver too, so a change to a layer
//! reaches the plugin's rows and every native surface at once. The layers
//! themselves, in order, stopping at the first hit:
//!
//! 1. **Exact id** — an entry whose id is `<app_id>.desktop`, or the same
//!    with the app id lowercased.
//! 2. **Id containment** — an entry whose id without `.desktop`, lowercased,
//!    contains the lowercased app id or is contained in it. This is the layer
//!    that catches the common cases: reverse-DNS spellings
//!    (`org.gnome.nautilus` for `org.gnome.Nautilus.desktop`), NixOS wrapper
//!    names (`firefox-unwrapped`), niri's spawn scopes (`niri-firefox`), and
//!    a lowercase cgroup leaf (`firefox`) for `Firefox.desktop`.
//! 3. **Executable basename** — an entry whose `Exec=` program's file stem,
//!    lowercased, equals the lowercased app id. An entry with no `Exec=` (a
//!    `DBusActivatable=true` one may have none) has no program and is skipped,
//!    which is what retires #1434's crash along with the gio walk that had it.
//!
//! What stays in this file is the one thing a GTK widget needs and a GTK-free
//! resolver cannot hand over: a `gio::Icon`. `icon_from_desktop_value` builds
//! it from the resolver's raw `Icon=` value by GIO's own rule, so no
//! `AppInfo::all()` scan is left here at all.
//!
//! # One scan per page build (#1441)
//!
//! A lookup that misses the cache scans every `.desktop` file on the search
//! path, synchronously, on the GTK main thread. `resolve_app_meta` alone
//! does that once **per unseen app id**, so a Workspaces page with twelve
//! apps nobody had looked up cost twelve full scans — measured at about
//! 270 ms at 611 entries in a release build (#1439 review, L3). A page that
//! knows every id it is about to render hands them all to
//! `resolve_app_metas` first, which sends only the ids the cache lacks
//! through **one** [`Resolver::resolve_all`], and the page's per-row
//! `resolve_app_meta` calls are then all hits. The three pages that render
//! app ids from a list — `panels::workspaces`' columns (every column in one
//! batch), `panels::stats`' Top apps and `panels::workspace_edit`'s app list
//! — each do that once per rebuild. The Stats page has two lists, CPU and
//! RAM; they share one cache and each batches both lists' ids, so a page
//! build is still one scan. `components::app_picker` needs no batch:
//! `desktop_entry::installed` seeds its cache from the one scan that lists
//! the rows.
//!
//! The batch does not keep a [`Resolver`] alive between rebuilds. A resolver
//! keeps its answers, not the entries it scanned, so a new id would scan again
//! anyway; and one kept for the life of the cache (or the process) would keep
//! answering from before an app was installed.
//!
//! # The environment
//!
//! The resolver reads this process's environment
//! ([`desktop_entry::Env::from_process`]:
//! `XDG_DATA_HOME`, `HOME`, `XDG_DATA_DIRS`, `PATH` and the locale variables
//! `LANGUAGE`/`LC_ALL`/`LC_MESSAGES`/`LANG`) — the same variables GIO's scan
//! read before. The stats plugin's resolver reads **its** process's
//! environment, and a plugin is launched through `systemd-run --user`
//! (`plugin_launcher.rs`), so it gets the user manager's environment rather
//! than the shell's. So one copy of the lookup is still not one answer: the
//! plugin's rows and the native Stats page name an app the same only while
//! both processes see the same values. `hytte_sensors::desktop_entry`'s
//! "Known gaps" says how to compare the two.

use std::borrow::BorrowMut;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use hytte::gtk::{gio, prelude::*};
use hytte_sensors::desktop_entry::{self, Resolver};

/// A desktop entry's display name and icon, as the shell's widgets use them.
#[derive(Clone)]
pub(crate) struct AppMeta {
    /// `X-GNOME-FullName`, else `Name`, localised, else `Unnamed` — the
    /// resolver's [`desktop_entry::AppMeta::display_name`], unchanged.
    pub(crate) display_name: String,
    /// The entry's `Icon=` as a `gio::Icon` ([`icon_from_desktop_value`]);
    /// `None` when the entry has no `Icon=` line.
    pub(crate) icon: Option<gio::Icon>,
}

impl AppMeta {
    /// The resolver's answer with its `Icon=` value made a `gio::Icon`.
    fn from_entry(entry: &desktop_entry::AppMeta) -> Self {
        Self {
            display_name: entry.display_name.clone(),
            icon: entry.icon.as_deref().map(icon_from_desktop_value),
        }
    }
}

/// The caller-owned cache [`resolve_app_meta`] fills: app-id → resolution,
/// with `None` meaning "scanned, no desktop entry" so a miss also costs at
/// most one scan.
///
/// This **is** the lookup's cache (#1432): [`resolve_app_meta`] and
/// [`resolve_app_metas`] build a [`Resolver`] for each call that has a miss in
/// it and drop it straight after, so the resolver's own cache never outlives
/// the answers copied into this map. It stays the
/// shell's own map rather than a `Resolver` for two reasons: it holds the
/// built `gio::Icon`, a `GObject` the GTK-free resolver cannot, and callers
/// seed it (`components::desktop_entry::installed` fills it from the picker's
/// one scan; the Stats page's tests fill it so nothing reads the host), which
/// a `Resolver` has no way in for.
///
/// Caller-owned rather than a module global on purpose: the scan result is
/// only as fresh as the widget that holds it, and every consumer today scopes
/// it to one bind closure's lifetime.
///
/// **Borrow discipline** (#643/#663/#832): an argument-position `borrow_mut()`
/// is a temporary of the *whole enclosing statement*, so inlining
/// `resolve_app_meta(id, &mut cache.borrow_mut())` into a `format!` or a GTK
/// setter holds the `RefMut` across a call that can synchronously re-enter —
/// and a `BorrowMutError` unwinding through a glib callback aborts the
/// process. Resolve into a local first, set second.
pub(crate) type MetaCache = Rc<RefCell<HashMap<String, Option<AppMeta>>>>;

/// Resolve display name and icon for an app-id through
/// [`hytte_sensors::desktop_entry`], caching the result so the search path is
/// scanned at most once per unique app-id per cache lifetime.
///
/// A cache hit costs no scan at all: the cache is checked before the
/// environment is read or a [`Resolver`] exists. A miss reads the
/// environment, scans every `.desktop` file on the search path once — the
/// same work `gio::AppInfo::all()` did per miss before #1432, since GIO
/// re-parses every listed file on each call — and caches the answer, a miss
/// included.
///
/// A page rendering several app ids resolves them with [`resolve_app_metas`]
/// first, so this is a hit for each of them (#1441).
///
/// See the module docs for the three layers and the environment.
pub(crate) fn resolve_app_meta(
    app_id: &str,
    meta_cache: &mut HashMap<String, Option<AppMeta>>,
) -> Option<AppMeta> {
    resolve_app_meta_in(app_id, meta_cache, process_resolver)
}

/// Resolve every id in `app_ids` into `meta_cache` with **at most one scan**
/// for all the ids it does not hold yet (#1441), so the caller's per-row
/// [`resolve_app_meta`] calls that follow are all hits.
///
/// Exactly [`resolve_app_meta`]'s semantics, for many ids at once: the same
/// names and icons, a miss cached as `None`, and an id already in the cache
/// never sent to the resolver — so a batch the cache already answers reads no
/// environment and scans nothing. See the module docs for why a page calls
/// this once per rebuild and does not keep a [`Resolver`] instead.
///
/// Takes the cache as `&mut`, like [`resolve_app_meta`]: call it as a
/// statement of its own (`resolve_app_metas(ids, &mut cache.borrow_mut());`),
/// never inside a GTK setter — [`MetaCache`]'s borrow discipline.
pub(crate) fn resolve_app_metas<'a>(
    app_ids: impl IntoIterator<Item = &'a str>,
    meta_cache: &mut HashMap<String, Option<AppMeta>>,
) {
    resolve_app_metas_in(app_ids, meta_cache, process_resolver);
}

/// The resolver a production lookup scans with: [`Resolver::from_env`], over
/// this process's environment.
///
/// Under `cfg(test)` a thread that installed a fixture
/// (`test_support::Fixture::install`) gets a resolver over that fixture
/// instead, and the build is counted (`test_support::scans`) — how a page's
/// `#[gtk::test]` asks how many scans one rebuild cost without reading the
/// host's desktop entries. The same shape as `plugins::preem_gl::arm`'s
/// `TEST_ARM`. With no fixture installed, a test gets the real wrapper.
fn process_resolver() -> Resolver {
    #[cfg(test)]
    if let Some(resolver) = test_support::installed_resolver() {
        return resolver;
    }
    Resolver::from_env()
}

/// [`resolve_app_meta`] through the [`Resolver`] that `resolver` hands over —
/// a batch of one, through [`resolve_app_metas_in`], so the single lookup and
/// the batch are one code path.
fn resolve_app_meta_in<R: BorrowMut<Resolver>>(
    app_id: &str,
    meta_cache: &mut HashMap<String, Option<AppMeta>>,
    resolver: impl FnOnce() -> R,
) -> Option<AppMeta> {
    resolve_app_metas_in([app_id], meta_cache, resolver);
    meta_cache.get(app_id).cloned().flatten()
}

/// [`resolve_app_metas`] through the [`Resolver`] that `resolver` hands over,
/// which is called only when some id misses the cache, and at most once.
///
/// Production passes [`process_resolver`], so the resolver — and the
/// environment read that builds it — exists only for a batch with a miss in
/// it. A test passes a `&mut Resolver` over a fixture search path instead,
/// and reads [`Resolver::scans`] afterwards: the count is then taken on the
/// very resolver the lookup used, not inferred from how often something was
/// asked for (#1439 review N2). The seam hands this function no `Env`, so it
/// has nothing to build a second resolver from.
///
/// Only the misses reach the resolver, once each: [`Resolver::resolve_all`]
/// scans for all of them together, and the [`Resolver::resolve`] that copies
/// each answer out is a hit in the resolver's own cache, so it scans nothing.
fn resolve_app_metas_in<'a, R: BorrowMut<Resolver>>(
    app_ids: impl IntoIterator<Item = &'a str>,
    meta_cache: &mut HashMap<String, Option<AppMeta>>,
    resolver: impl FnOnce() -> R,
) {
    let mut misses: Vec<&str> = app_ids
        .into_iter()
        .filter(|app_id| !meta_cache.contains_key(*app_id))
        .collect();
    if misses.is_empty() {
        return;
    }
    // An app on two cards is one id, and one answer to copy out.
    misses.sort_unstable();
    misses.dedup();
    let mut resolver = resolver();
    let resolver = resolver.borrow_mut();
    resolver.resolve_all(misses.iter().copied());
    for app_id in misses {
        let meta = resolver.resolve(app_id).map(AppMeta::from_entry);
        meta_cache.insert(app_id.to_owned(), meta);
    }
}

/// A desktop entry's `Icon=` value as a `gio::Icon`, by GIO's own rule —
/// `g_desktop_app_info_load_from_keyfile` in the `GLib` the shell links
/// (2.88.3, `gio/gdesktopappinfo.c:2033-2057`), i.e. how GIO built the icon
/// `gio::AppInfo::icon()` handed this file before #1432:
///
/// - **An absolute path** (`g_path_is_absolute`: the first byte is `/` on
///   Unix, `glib/gfileutils.c:2473-2488`) becomes a `gio::FileIcon` for that
///   path, extension and all.
/// - **Anything else** becomes a `gio::ThemedIcon`, after one trailing
///   `.png`, `.xpm` or `.svg` is dropped — GIO's "common mistake in desktop
///   files" workaround. The match is `strrchr` + `strcmp`, i.e. the text
///   after the **last** dot, case-sensitive: `firefox.png` is `firefox`,
///   `a.svg.png` is `a.svg`, and `org.gnome.Nautilus` and `foo.PNG` are kept
///   whole. An empty `Icon=` is a themed icon named `""`, as in GIO.
///   `g_themed_icon_new`, not the `_with_default_fallbacks` variant.
///
/// The value arrives already localised (`Icon[sv]=` over `Icon=`), as GIO
/// reads it with `g_key_file_get_locale_string`.
fn icon_from_desktop_value(value: &str) -> gio::Icon {
    if value.starts_with('/') {
        return gio::FileIcon::new(&gio::File::for_path(value)).upcast();
    }
    let name = [".png", ".xpm", ".svg"]
        .into_iter()
        .find_map(|extension| value.strip_suffix(extension))
        .unwrap_or(value);
    gio::ThemedIcon::new(name).upcast()
}

/// The icon shown for an app-id that resolves to no desktop entry (or to an
/// entry carrying no icon of its own).
pub(crate) fn fallback_icon() -> gio::Icon {
    gio::ThemedIcon::new("application-x-executable-symbolic").upcast::<gio::Icon>()
}

/// The desktop-entry fixture shared by this module's tests, by the pages that
/// batch their lookups through it (#1441) and by the shell's gio scans'
/// non-UTF-8 tests (#1441 item 2).
///
/// Two ways in, and neither reads the host's desktop entries:
///
/// - **In process**, through the resolver's plain data: [`Fixture::env`]
///   hands a test's own `Resolver` the fixture search path, and
///   [`Fixture::install`] points every *production* lookup on the calling
///   thread at it for as long as the guard lives, counting each resolver it
///   builds ([`scans`]). That second one is how a page's `#[gtk::test]` asks
///   how many scans one rebuild cost.
/// - **In a child process**, through the real environment:
///   [`Fixture::run_child`], for the tests that need GIO itself, or
///   [`resolve_app_meta`](super::resolve_app_meta)'s real wrapper over
///   `Env::from_process`. `std::env::set_var` is `unsafe` in edition 2024 and
///   this workspace forbids `unsafe_code`, so the fixture is set on the
///   **child** through the safe `Command::env` builder — the precedent is
///   `hytte-plugin-stats::plugin::tests::settings_reads_the_real_process_environment`,
///   and #1435 brought it here. No display is involved (`gio::AppInfo::all()`
///   and the icon constructors are plain GIO). The child's data, config and
///   `HOME` directories all sit under the fixture root.
#[cfg(test)]
pub(crate) mod test_support {
    use std::cell::{Cell, RefCell};
    use std::ffi::OsStr;
    use std::marker::PhantomData;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    use hytte::gtk::{gio, prelude::*};
    use hytte_sensors::desktop_entry::{Env, Resolver};

    /// [`Fixture::non_utf8_entry`]'s file name.
    const NON_UTF8_NAME: &[u8] = b"ts-\xff\xfe.desktop";

    thread_local! {
        /// The search path this thread's production lookups scan instead of
        /// the process environment's, while an [`Installed`] guard lives.
        static INSTALLED: RefCell<Option<Env>> = const { RefCell::new(None) };
        /// Resolvers built over [`INSTALLED`] since it was installed.
        static SCANS: Cell<usize> = const { Cell::new(0) };
    }

    /// A resolver over the installed fixture, counted — `None` when this
    /// thread has none installed, so production's wrapper runs.
    pub(super) fn installed_resolver() -> Option<Resolver> {
        let env = INSTALLED.with_borrow(Clone::clone)?;
        SCANS.set(SCANS.get() + 1);
        Some(Resolver::new(env))
    }

    /// How many scans this thread's production lookups made since the fixture
    /// was installed.
    ///
    /// Counted as resolvers built, which is the same number:
    /// `resolve_app_metas_in` builds one only for a batch with a miss in it
    /// and scans it exactly once, which
    /// `a_batch_of_unseen_ids_scans_once` pins on the resolver itself.
    pub(crate) fn scans() -> usize {
        SCANS.get()
    }

    /// Keeps a fixture installed on this thread; dropping it (a panic
    /// included) puts production's wrapper back. Not `Send`: the fixture is
    /// per thread. Borrows its [`Fixture`], so the compiler refuses a guard
    /// that would outlive the directory it points the lookups at (#1443
    /// review, N2) — a lookup there would otherwise fail silently as "no
    /// entry".
    #[must_use = "the fixture is uninstalled when this guard drops"]
    pub(crate) struct Installed<'f> {
        _fixture: PhantomData<(&'f Fixture, *const ())>,
    }

    impl Drop for Installed<'_> {
        fn drop(&mut self) {
            INSTALLED.set(None);
        }
    }

    /// A fixture search path on disk: entries under
    /// `<root>/data-home/applications/`, and `<root>/bin/` as the whole
    /// `$PATH`, holding the programs their `Exec=` lines name — GIO and the
    /// resolver both drop an entry whose program is not there.
    pub(crate) struct Fixture {
        root: tempfile::TempDir,
    }

    impl Fixture {
        pub(crate) fn new() -> Self {
            Self {
                root: tempfile::tempdir().expect("a scratch fixture root"),
            }
        }

        pub(crate) fn root(&self) -> &Path {
            self.root.path()
        }

        fn bin(&self) -> PathBuf {
            self.root().join("bin")
        }

        fn data_home(&self) -> PathBuf {
            self.root().join("data-home")
        }

        /// An executable at `bin/<name>`, returning its absolute path.
        pub(crate) fn program(&self, name: &str) -> PathBuf {
            let path = self.bin().join(name);
            std::fs::create_dir_all(self.bin()).expect("mkdir the fixture bin/");
            std::fs::write(&path, "#!/bin/sh\n").expect("write a fixture program");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .expect("chmod a fixture program");
            path
        }

        /// `contents` at `data-home/applications/<file_name>`.
        pub(crate) fn entry(&self, file_name: impl AsRef<Path>, contents: &str) {
            let apps = self.data_home().join("applications");
            std::fs::create_dir_all(&apps).expect("mkdir the fixture applications/");
            std::fs::write(apps.join(file_name), contents).expect("write a fixture .desktop file");
        }

        /// A listable entry whose **file name** is not UTF-8 —
        /// `ts-\xff\xfe.desktop`, the #1439 review's reproduction (L4) — with
        /// its `Exec=` program on the fixture `PATH`, so GIO lists it. Calling
        /// `gio::AppInfo`'s `id()` on it trips glib-rs's
        /// `debug_assert!(… "C string is not valid utf-8")`
        /// (`glib-0.22.5/src/gstring.rs:650`), so a gio scan that reaches it
        /// unguarded panics a debug build.
        pub(crate) fn non_utf8_entry(&self) {
            self.program("ts-1441-bad-name");
            self.entry(
                OsStr::from_bytes(NON_UTF8_NAME),
                "[Desktop Entry]\nType=Application\nName=TS 1441 Bad Name\nExec=ts-1441-bad-name\n",
            );
        }

        /// Test setup, in a [`Fixture::run_child`] child: GIO really lists the
        /// [`Fixture::non_utf8_entry`], so a scan that skips it has something
        /// to skip. Read through the entry's `filename` property as a
        /// `PathBuf` — raw bytes, the one accessor that is safe on it.
        pub(crate) fn assert_gio_lists_the_non_utf8_entry() {
            let listed = gio::AppInfo::all().iter().any(|info| {
                info.property::<Option<PathBuf>>("filename")
                    .is_some_and(|path| path.file_name() == Some(OsStr::from_bytes(NON_UTF8_NAME)))
            });
            assert!(listed, "test setup: GIO must list the non-UTF-8 file name");
        }

        /// This search path as the resolver's plain data, for the in-process
        /// tests: no language preference, so no entry is read translated.
        pub(crate) fn env(&self) -> Env {
            Env {
                dirs: vec![self.data_home().join("applications")],
                path: vec![self.bin()],
                languages: Vec::new(),
            }
        }

        /// Point this thread's production lookups ([`resolve_app_meta`],
        /// [`resolve_app_metas`]) at [`Fixture::env`] until the guard drops,
        /// with [`scans`] starting from zero.
        ///
        /// [`resolve_app_meta`]: super::resolve_app_meta
        /// [`resolve_app_metas`]: super::resolve_app_metas
        pub(crate) fn install(&self) -> Installed<'_> {
            INSTALLED.set(Some(self.env()));
            SCANS.set(0);
            Installed {
                _fixture: PhantomData,
            }
        }

        /// Re-exec this test binary running only `inner`, with `marker` set to
        /// the fixture root and the whole environment GIO and the resolver
        /// read pointed at the fixture — the data and config directories,
        /// `HOME`, `PATH`, and `LANGUAGE=sv` with the other locale variables
        /// removed. Returns the child's stdout once it exited 0 **and** printed
        /// `ok` (a stale filter matches no test, and libtest still exits 0).
        pub(crate) fn run_child(&self, inner: &str, marker: &str, ok: &str) -> String {
            let out = std::process::Command::new(
                std::env::current_exe().expect("this test binary's own path"),
            )
            .args(["--exact", "--nocapture", "--test-threads=1", inner])
            .env(marker, self.root())
            .env("XDG_DATA_HOME", self.data_home())
            .env("XDG_DATA_DIRS", self.root().join("data-sys"))
            .env("XDG_CONFIG_HOME", self.root().join("config-home"))
            .env("XDG_CONFIG_DIRS", self.root().join("config-sys"))
            .env("HOME", self.root())
            .env("PATH", self.bin())
            .env("LANGUAGE", "sv")
            .env_remove("LC_ALL")
            .env_remove("LC_MESSAGES")
            .env_remove("LANG")
            .output()
            .expect("re-exec this test binary with a fixture environment");
            let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(
                out.status.success(),
                "the child failed ({:?})\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
                out.status,
            );
            assert!(
                stdout.contains(ok),
                "the child exited 0 without reaching the end of {inner}\n\
                 --- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
            );
            stdout
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;

    use hytte::gtk::{gio, glib, prelude::*};
    use hytte_sensors::desktop_entry::Resolver;

    use super::test_support::{self, Fixture};
    use super::{
        AppMeta, icon_from_desktop_value, resolve_app_meta, resolve_app_meta_in, resolve_app_metas,
        resolve_app_metas_in,
    };
    use crate::components::desktop_entry;

    /// A resolver that must never be built. Production's is
    /// `Resolver::from_env`, so not building one is not reading the
    /// environment either.
    fn no_resolver() -> Resolver {
        panic!("a cache hit must neither build a resolver nor scan")
    }

    /// **A hit costs no scan** — it does not even build a resolver (so, in
    /// production, does not read the environment), for a found entry and for
    /// a cached miss alike.
    ///
    /// Falsified by deleting the cache check in [`resolve_app_meta_in`]: the
    /// resolver is then built on every call and [`no_resolver`] panics.
    #[test]
    fn a_cache_hit_reads_no_environment_and_scans_nothing() {
        let mut cache = HashMap::new();
        cache.insert(
            "seeded".to_owned(),
            Some(AppMeta {
                display_name: "Seeded".to_owned(),
                icon: None,
            }),
        );
        cache.insert("seeded-miss".to_owned(), None);

        let hit = resolve_app_meta_in("seeded", &mut cache, no_resolver);
        assert_eq!(hit.map(|m| m.display_name).as_deref(), Some("Seeded"));
        assert!(resolve_app_meta_in("seeded-miss", &mut cache, no_resolver).is_none());
    }

    /// **A miss scans exactly once and is cached, found or not** — counted
    /// on the resolver itself (#1439 review N2): each lookup is handed a
    /// fresh resolver over the fixture, and [`Resolver::scans`] is read
    /// afterwards, so a miss must leave it at 1 and a hit at 0.
    ///
    /// Falsified by dropping the `meta_cache.insert` (the repeat scans: 1 ≠
    /// 0), by caching only a found entry (the repeated miss scans), and by a
    /// second scan on the miss path (2 ≠ 1).
    #[test]
    fn a_miss_scans_once_and_is_cached_found_or_not() {
        let f = Fixture::new();
        f.program("ts-hit");
        f.entry(
            "ts-hit.desktop",
            "[Desktop Entry]\nType=Application\nName=TS Hit\nExec=ts-hit\n",
        );
        let mut cache = HashMap::new();

        for (app_id, name, scans) in [
            ("ts-hit", Some("TS Hit"), 1),
            ("ts-hit", Some("TS Hit"), 0),
            ("no-such-app-anywhere", None, 1),
            ("no-such-app-anywhere", None, 0),
        ] {
            let mut resolver = Resolver::new(f.env());
            let handed = &mut resolver;
            let meta = resolve_app_meta_in(app_id, &mut cache, move || handed);
            assert_eq!(meta.map(|m| m.display_name).as_deref(), name, "{app_id}");
            assert_eq!(resolver.scans(), scans, "{app_id}: scans for this lookup");
        }
        assert!(
            matches!(cache.get("no-such-app-anywhere"), Some(None)),
            "the miss is cached as `None`"
        );
    }

    /// **A mixed-case id is a hit on its second lookup too.** niri reports
    /// most GTK apps by a reverse-DNS id (`org.gnome.Nautilus`), so a cache
    /// keyed by anything but the id as given would scan on every render.
    ///
    /// From the #1439 review (T1), adapted to count on each lookup's own
    /// resolver. Falsified by keying the cache on `app_id.to_lowercase()`
    /// (3 scans ≠ 1).
    #[test]
    fn a_mixed_case_id_is_cached_under_its_own_spelling() {
        let f = Fixture::new();
        f.program("ts-hit");
        f.entry(
            "Org.Example.Mixed.desktop",
            "[Desktop Entry]\nType=Application\nName=TS Mixed\nExec=ts-hit\n",
        );
        let mut cache = HashMap::new();
        let mut scans = 0;
        for _ in 0..3 {
            let mut resolver = Resolver::new(f.env());
            let handed = &mut resolver;
            let meta = resolve_app_meta_in("Org.Example.Mixed", &mut cache, move || handed);
            assert_eq!(meta.map(|m| m.display_name).as_deref(), Some("TS Mixed"));
            scans += resolver.scans();
        }
        assert_eq!(scans, 1, "a mixed-case id is scanned for once");
    }

    /// Twelve app ids one page might render at once, and the name each must
    /// resolve to over [`batch_fixture`]: every layer, and three misses.
    const BATCH: [(&str, Option<&str>); 12] = [
        // Layer 1, as written and lowercased.
        ("org.example.Files", Some("Files")),
        ("TS-Term", Some("Terminal")),
        ("ts-term", Some("Terminal")),
        // Layer 2: another case, a cgroup leaf, a spawn scope, a wrapper, a
        // last dotted part.
        ("org.example.files", Some("Files")),
        ("firefox", Some("Firefox")),
        ("niri-firefox", Some("Firefox")),
        ("firefox-unwrapped", Some("Firefox")),
        ("editor", Some("Editor")),
        // Layer 3: the `Exec=` program's stem.
        ("ts-edit", Some("Editor")),
        // No entry at all.
        ("no-such-app-1441-a", None),
        ("no-such-app-1441-b", None),
        ("no-such-app-1441-c", None),
    ];

    /// The entries [`BATCH`] resolves against.
    fn batch_fixture() -> Fixture {
        let f = Fixture::new();
        for (file, name, program) in [
            ("org.example.Files.desktop", "Files", "ts-files"),
            ("Firefox.desktop", "Firefox", "firefox"),
            ("ts-term.desktop", "Terminal", "ts-term"),
            ("com.example.Editor.desktop", "Editor", "ts-edit"),
        ] {
            f.program(program);
            f.entry(
                file,
                &format!("[Desktop Entry]\nType=Application\nName={name}\nExec={program}\n"),
            );
        }
        f
    }

    /// **A batch of unseen ids costs one scan** (#1441), counted on the
    /// resolver the batch used, and leaves every id answered in the cache — a
    /// miss as `None` — so the per-row lookups that follow never build a
    /// resolver. An id the batch names twice is still one answer.
    ///
    /// Falsified by resolving each miss on its own (the `resolve_all` dropped,
    /// so each `resolve` copy-out scans: 12 ≠ 1), and by caching only the
    /// found ids (a miss's row lookup reaches [`no_resolver`]).
    #[test]
    fn a_batch_of_unseen_ids_scans_once() {
        let f = batch_fixture();
        let mut cache = HashMap::new();
        let mut resolver = Resolver::new(f.env());
        let handed = &mut resolver;
        let ids = BATCH.iter().map(|(app_id, _)| *app_id).chain(["firefox"]);
        resolve_app_metas_in(ids, &mut cache, move || handed);
        assert_eq!(resolver.scans(), 1, "twelve unseen ids, one scan");
        assert_eq!(cache.len(), BATCH.len(), "one answer per id");
        for (app_id, name) in BATCH {
            let meta = resolve_app_meta_in(app_id, &mut cache, no_resolver);
            assert_eq!(meta.map(|m| m.display_name).as_deref(), name, "{app_id}");
        }
    }

    /// **A batch the cache already answers scans nothing** — it does not even
    /// build a resolver, whether the ids were seeded, answered by an earlier
    /// batch, or a cached miss — and leaves their answers alone.
    ///
    /// Falsified by dropping the early return on an empty miss list (the
    /// resolver is built anyway: [`no_resolver`] panics).
    #[test]
    fn a_batch_the_cache_already_answers_scans_nothing() {
        let f = batch_fixture();
        let mut cache = HashMap::new();
        cache.insert(
            "seeded".to_owned(),
            Some(AppMeta {
                display_name: "Seeded".to_owned(),
                icon: None,
            }),
        );
        cache.insert("seeded-miss".to_owned(), None);
        resolve_app_metas_in(["seeded", "seeded-miss", "seeded"], &mut cache, no_resolver);
        resolve_app_metas_in(std::iter::empty(), &mut cache, no_resolver);

        let mut resolver = Resolver::new(f.env());
        let handed = &mut resolver;
        resolve_app_metas_in(BATCH.map(|(app_id, _)| app_id), &mut cache, move || handed);
        assert_eq!(resolver.scans(), 1);
        resolve_app_metas_in(BATCH.map(|(app_id, _)| app_id), &mut cache, no_resolver);

        assert_eq!(
            cache["seeded"].as_ref().map(|m| m.display_name.as_str()),
            Some("Seeded")
        );
        assert!(matches!(cache.get("seeded-miss"), Some(None)));
        assert_eq!(cache.len(), BATCH.len() + 2);
    }

    /// **A mixed batch scans once, for its misses only.** The ids the cache
    /// holds keep the answers it holds — seeded here with answers the fixture
    /// would not give, so a re-resolve would show — and the resolver is never
    /// told about them: asking it one afterwards scans again, while a miss it
    /// was sent is already its own hit.
    ///
    /// Falsified by sending every id through the resolver, cached or not (the
    /// seeded `firefox` comes back `Firefox`, and asking the resolver about it
    /// afterwards scans nothing).
    #[test]
    fn a_mixed_batch_scans_once_for_its_misses_only() {
        let f = batch_fixture();
        let mut cache = HashMap::new();
        cache.insert(
            "firefox".to_owned(),
            Some(AppMeta {
                display_name: "Seeded Firefox".to_owned(),
                icon: None,
            }),
        );
        cache.insert("ts-term".to_owned(), None);

        let mut resolver = Resolver::new(f.env());
        let handed = &mut resolver;
        resolve_app_metas_in(
            [
                "firefox",
                "ts-edit",
                "ts-term",
                "editor",
                "no-such-app-1441-a",
            ],
            &mut cache,
            move || handed,
        );
        assert_eq!(resolver.scans(), 1, "three misses, one scan");

        let name = |cache: &HashMap<String, Option<AppMeta>>, app_id: &str| {
            cache[app_id].as_ref().map(|m| m.display_name.clone())
        };
        assert_eq!(name(&cache, "firefox").as_deref(), Some("Seeded Firefox"));
        assert_eq!(name(&cache, "ts-term"), None, "a cached miss stays one");
        assert_eq!(name(&cache, "ts-edit").as_deref(), Some("Editor"));
        assert_eq!(name(&cache, "editor").as_deref(), Some("Editor"));
        assert_eq!(name(&cache, "no-such-app-1441-a"), None);

        for (app_id, scans) in [
            ("ts-edit", 1),
            ("editor", 1),
            ("no-such-app-1441-a", 1),
            ("firefox", 2),
            ("ts-term", 3),
        ] {
            resolver.resolve(app_id);
            assert_eq!(
                resolver.scans(),
                scans,
                "{app_id}: was it sent to the resolver?"
            );
        }
    }

    /// **An installed fixture answers the production wrappers and counts
    /// their scans** — the seam the three pages' `#[gtk::test]`s count one
    /// rebuild's scans through (#1441), pinned here without a display. While
    /// the guard lives, the real [`resolve_app_metas`] and
    /// [`resolve_app_meta`] resolve against the fixture, a batch with a miss
    /// is one counted scan, and the per-row lookups after it are hits; once
    /// the guard drops, production's wrapper is back.
    ///
    /// Falsified by `process_resolver` ignoring the installed fixture (the
    /// names come from wherever the process environment points, and nothing
    /// is counted), and by the guard not uninstalling on drop.
    #[test]
    fn an_installed_fixture_answers_the_production_wrappers_and_counts_their_scans() {
        let f = batch_fixture();
        {
            let _installed = f.install();
            let mut cache = HashMap::new();
            resolve_app_metas(BATCH.map(|(app_id, _)| app_id), &mut cache);
            assert_eq!(test_support::scans(), 1, "one batch, one scan");
            for (app_id, name) in BATCH {
                let meta = resolve_app_meta(app_id, &mut cache);
                assert_eq!(meta.map(|m| m.display_name).as_deref(), name, "{app_id}");
            }
            assert_eq!(test_support::scans(), 1, "the per-row lookups are hits");
            assert!(resolve_app_meta("ts-1441-late", &mut cache).is_none());
            assert_eq!(test_support::scans(), 2, "a lookup of a new id scans");
        }
        assert!(
            test_support::installed_resolver().is_none(),
            "the guard uninstalls the fixture when it drops"
        );
    }

    const FRESH_CHILD: &str = "TROLLSHELL_APP_META_1432_FRESH_CHILD";
    const FRESH_CHILD_OK: &str = "app-meta-1432-fresh-child-reached-the-end";

    /// **A new cache sees an entry installed after an old cache's miss**,
    /// through the real wrapper; the old cache keeps its miss. That is the
    /// freshness the gio path had, and what a process-wide resolver loses.
    ///
    /// From the #1439 review (T2). Falsified by one `thread_local!`
    /// `Resolver` shared by every call (the new cache is answered from the
    /// old resolver's miss).
    #[test]
    fn a_new_cache_sees_an_entry_installed_after_a_miss() {
        let f = Fixture::new();
        f.program("ts-late");
        f.entry(
            "ts-other.desktop",
            "[Desktop Entry]\nType=Application\nName=Other\nExec=ts-late\n",
        );
        f.run_child(
            "components::app_meta::tests::a_new_cache_sees_an_entry_installed_after_a_miss_inner",
            FRESH_CHILD,
            FRESH_CHILD_OK,
        );
    }

    /// The child half of the test above; a no-op outside its child.
    #[test]
    fn a_new_cache_sees_an_entry_installed_after_a_miss_inner() {
        let Some(root) = std::env::var_os(FRESH_CHILD) else {
            return;
        };
        let mut old = HashMap::new();
        assert!(resolve_app_meta("ts-late-app", &mut old).is_none());
        std::fs::write(
            Path::new(&root).join("data-home/applications/ts-late-app.desktop"),
            "[Desktop Entry]\nType=Application\nName=TS Late\nExec=ts-late\n",
        )
        .expect("install the late entry");
        assert!(
            resolve_app_meta("ts-late-app", &mut old).is_none(),
            "a cached miss stays a miss for its cache's life"
        );
        let mut new = HashMap::new();
        assert_eq!(
            resolve_app_meta("ts-late-app", &mut new)
                .map(|m| m.display_name)
                .as_deref(),
            Some("TS Late"),
            "a fresh cache reads the entry installed since"
        );
        println!("{FRESH_CHILD_OK}");
    }

    /// What an icon is, in the terms the comparison below can see: its
    /// `GType`, `g_icon_to_string` and the printed `g_icon_serialize`.
    fn describe(icon: Option<&gio::Icon>) -> Option<(String, Option<String>, Option<String>)> {
        icon.map(|icon| {
            (
                icon.type_().name().to_owned(),
                IconExt::to_string(icon).map(|s| s.to_string()),
                icon.serialize().map(|v| v.print(true).to_string()),
            )
        })
    }

    /// **GIO's icon rule, arm by arm**, as literals — the parity test below
    /// holds the same rule to GIO's own `AppInfo::icon()`; this one says which
    /// arm each spelling takes.
    ///
    /// Falsified by dropping the absolute-path arm (the path becomes a themed
    /// name), and by dropping any one extension from the list (that spelling
    /// keeps it).
    #[test]
    fn the_icon_rule_is_gios_arm_by_arm() {
        for (value, file) in [
            ("/opt/icons/app.png", "/opt/icons/app.png"),
            ("/opt/icons/editor.svg", "/opt/icons/editor.svg"),
        ] {
            let icon = icon_from_desktop_value(value);
            let path = icon
                .downcast_ref::<gio::FileIcon>()
                .unwrap_or_else(|| panic!("{value}: an absolute path is a file icon"))
                .file()
                .path();
            assert_eq!(path.as_deref(), Some(Path::new(file)), "{value}");
        }
        for (value, name) in [
            ("firefox.png", "firefox"),
            ("editor.xpm", "editor"),
            ("editor.svg", "editor"),
            ("a.svg.png", "a.svg"),
            ("org.gnome.Nautilus", "org.gnome.Nautilus"),
            ("foo.PNG", "foo.PNG"),
            ("icons/rel.png", "icons/rel"),
            ("firefox", "firefox"),
        ] {
            let icon = icon_from_desktop_value(value);
            let names = icon
                .downcast_ref::<gio::ThemedIcon>()
                .unwrap_or_else(|| panic!("{value}: anything else is a themed icon"))
                .names();
            assert_eq!(
                names.first().map(glib::GString::as_str),
                Some(name),
                "{value}"
            );
        }
    }

    const PARITY_CHILD: &str = "TROLLSHELL_APP_META_1432_PARITY_CHILD";
    const PARITY_CHILD_OK: &str = "app-meta-1432-parity-child-reached-the-end";

    /// The app ids the module docs name, and the name each must come out as
    /// under the parity fixture (`LANGUAGE=sv`). These are the answers the
    /// gio layers this file carried before #1432 gave over the same fixture
    /// (a one-off oracle run on that code, recorded on #1432's PR), so they
    /// pin the old gio path's behaviour, not just the resolver's.
    const NAME_CASES: [(&str, Option<&str>); 8] = [
        // Layer 1, localised: `Name[sv]=`.
        ("org.gnome.Nautilus", Some("Filer")),
        // Layer 2: a reverse-DNS id in another case.
        ("org.gnome.nautilus", Some("Filer")),
        // Layer 2: a lowercase cgroup leaf for `Firefox.desktop`.
        ("firefox", Some("Firefox")),
        // Layer 2: niri's spawn scope.
        ("niri-firefox", Some("Firefox")),
        // Layer 2: the NixOS wrapper name.
        ("firefox-unwrapped", Some("Firefox")),
        // Layer 3: the `Exec=` program's file stem.
        ("ts-edit", Some("Editor")),
        // The no-`Exec=` entry itself, by id (#1434).
        ("ts-dbus-only", Some("DBus Only")),
        // Every layer, the third across the no-`Exec=` entry (#1434).
        ("unrelated-app-id-1434", None),
    ];

    /// The parity fixture's entries, by desktop-file id without `.desktop`;
    /// each covers one icon spelling, and every one resolves through layer 1
    /// under its own id.
    const ENTRY_STEMS: [&str; 12] = [
        "org.gnome.Nautilus",
        "Firefox",
        "com.example.Editor",
        "ts-dbus-only",
        "ts-xpm-icon",
        "ts-svg-icon",
        "ts-upper-icon",
        "ts-empty-icon",
        "ts-localised-icon",
        "ts-dotted-icon",
        "ts-relative-icon",
        "ts-abs-png-icon",
    ];

    /// The parity fixture: one entry per [`ENTRY_STEMS`] row.
    fn parity_fixture() -> Fixture {
        let f = Fixture::new();
        f.program("nautilus");
        f.program("firefox");
        f.program("ts-icon-host");
        let editor = f.program("ts-edit");
        let icons = f.root().join("icons");
        f.entry(
            "org.gnome.Nautilus.desktop",
            "[Desktop Entry]\nType=Application\nName=Files\nName[sv]=Filer\n\
             Exec=nautilus --new-window %U\nIcon=org.gnome.Nautilus\n",
        );
        f.entry(
            "Firefox.desktop",
            "[Desktop Entry]\nType=Application\nName=Firefox\nExec=firefox %u\nIcon=firefox.png\n",
        );
        f.entry(
            "com.example.Editor.desktop",
            &format!(
                "[Desktop Entry]\nType=Application\nName=Editor\nExec={} %F\nIcon={}\n",
                editor.display(),
                icons.join("ts-editor.svg").display(),
            ),
        );
        f.entry(
            "ts-dbus-only.desktop",
            "[Desktop Entry]\nType=Application\nName=DBus Only\nDBusActivatable=true\n",
        );
        let abs_png = icons.join("ts-abs.png");
        for (stem, icon) in [
            ("ts-xpm-icon", "Icon=ts-xpm-icon.xpm\n"),
            ("ts-svg-icon", "Icon=ts-svg-icon.svg\n"),
            ("ts-upper-icon", "Icon=ts-upper-icon.PNG\n"),
            ("ts-empty-icon", "Icon=\n"),
            (
                "ts-localised-icon",
                "Icon=ts-plain\nIcon[sv]=ts-svensk.png\n",
            ),
            ("ts-dotted-icon", "Icon=ts.dotted.name.png\n"),
            ("ts-relative-icon", "Icon=icons/ts-rel.png\n"),
            ("ts-abs-png-icon", &format!("Icon={}\n", abs_png.display())),
        ] {
            f.entry(
                format!("{stem}.desktop"),
                &format!(
                    "[Desktop Entry]\nType=Application\nName=Entry {stem}\nExec=ts-icon-host\n{icon}"
                ),
            );
        }
        f
    }

    /// **The shell names what the resolver names, and names and icons what
    /// GIO does.** In one fixture environment:
    ///
    /// - every [`NAME_CASES`] id resolves through [`resolve_app_meta`] — the
    ///   real wrapper over the process environment — to the expected name,
    ///   and to exactly what `hytte_sensors::desktop_entry::Resolver` answers;
    /// - every fixture entry resolves to the display name and the icon
    ///   **GIO's own** `AppInfo` for that desktop-file id carries, compared
    ///   by `GType`, `g_icon_to_string` and `g_icon_serialize`. That covers an
    ///   absolute path (with and without an extension), a themed name, one
    ///   with each stripped extension, one with an extension GIO keeps, a
    ///   dotted name, a relative path, a localised `Icon[sv]=`, an empty
    ///   `Icon=`, and no `Icon=` at all;
    /// - [`resolve_app_metas`], the batch's own real wrapper (#1441), answers
    ///   every one of those ids exactly as the one-at-a-time lookups did.
    ///
    /// Falsified by a wrapper reading no environment (`Env::default`), by
    /// naming rows from the raw id, and by either icon arm going wrong — see
    /// the PR's mutation table — and, for the batch, by its wrapper reading
    /// no environment either.
    #[test]
    fn names_match_the_resolver_and_icons_match_gio() {
        let f = parity_fixture();
        let stdout = f.run_child(
            "components::app_meta::tests::names_match_the_resolver_and_icons_match_gio_inner",
            PARITY_CHILD,
            PARITY_CHILD_OK,
        );
        println!("{stdout}");
    }

    /// The child half of the test above; a no-op outside its child.
    #[test]
    fn names_match_the_resolver_and_icons_match_gio_inner() {
        if std::env::var_os(PARITY_CHILD).is_none() {
            return;
        }
        let mut cache = HashMap::new();
        let mut resolver = Resolver::from_env();
        for (app_id, expected) in NAME_CASES {
            let shell = resolve_app_meta(app_id, &mut cache).map(|m| m.display_name);
            let sensors = resolver.resolve(app_id).map(|m| m.display_name.clone());
            println!("{app_id:>22}: shell {shell:?}, resolver {sensors:?}");
            assert_eq!(shell.as_deref(), expected, "{app_id}");
            assert_eq!(
                shell, sensors,
                "{app_id}: the shell and the resolver disagree"
            );
        }

        for stem in ENTRY_STEMS {
            let id = format!("{stem}.desktop");
            let info = desktop_entry::listed(&id)
                .unwrap_or_else(|| panic!("test setup: GIO must list the fixture {id}"));
            let ours = resolve_app_meta(stem, &mut cache)
                .unwrap_or_else(|| panic!("{stem} must resolve to its own entry"));
            let gio_icon = describe(info.icon().as_ref());
            let our_icon = describe(ours.icon.as_ref());
            println!("{id:>28}: gio {gio_icon:?}\n{:>28}  ours {our_icon:?}", "");
            assert_eq!(ours.display_name, info.display_name().as_str(), "{id}");
            assert_eq!(our_icon, gio_icon, "{id}: the icon differs from GIO's");
        }

        // #1441: the batch, through its own real wrapper into a fresh cache,
        // answers every id — names, icons and misses — as the one-at-a-time
        // lookups above did.
        let mut batched = HashMap::new();
        let ids = NAME_CASES
            .map(|(app_id, _)| app_id)
            .into_iter()
            .chain(ENTRY_STEMS);
        resolve_app_metas(ids.clone(), &mut batched);
        for app_id in ids {
            let answer = |cache: &HashMap<String, Option<AppMeta>>| {
                let meta = cache
                    .get(app_id)
                    .unwrap_or_else(|| panic!("{app_id} was not resolved"));
                meta.as_ref()
                    .map(|m| (m.display_name.clone(), describe(m.icon.as_ref())))
            };
            assert_eq!(
                answer(&batched),
                answer(&cache),
                "{app_id}: batched vs one at a time"
            );
        }
        println!("{PARITY_CHILD_OK}");
    }

    const NO_EXEC_CHILD: &str = "TROLLSHELL_APP_META_1434_NO_EXEC_CHILD";
    const NO_EXEC_CHILD_OK: &str = "app-meta-1434-no-exec-child-reached-the-end";

    /// **#1434, through the new path**: a `DBusActivatable=true` entry with no
    /// `Exec=` line resolves by its id, and an id that falls through to
    /// layer 3 across it misses — neither crashes.
    ///
    /// Before #1432 this crash lived in the gio walk's layer 3, which called
    /// `gio::AppInfo::executable()` — a `NULL` path for such an entry, which
    /// gio-rs does not bind as nullable (a debug-build abort, a release-build
    /// null dereference). #1435 guarded that call; #1432 deleted the walk,
    /// and the resolver's layer 3 skips an entry with no executable.
    ///
    /// Falsified twice: by making the resolver's layer 3 match an entry with
    /// no executable (the unrelated id then names `TS 1434 No Exec`), and by
    /// putting an unguarded `executable()` walk back into this file (the child
    /// aborts in glib-rs's `debug_assert!`).
    #[test]
    fn a_desktop_entry_with_no_exec_resolves_without_crashing() {
        let f = Fixture::new();
        f.entry(
            "ts-1434-no-exec.desktop",
            "[Desktop Entry]\nType=Application\nName=TS 1434 No Exec\nDBusActivatable=true\n",
        );
        f.run_child(
            "components::app_meta::tests::a_desktop_entry_with_no_exec_resolves_without_crashing_inner",
            NO_EXEC_CHILD,
            NO_EXEC_CHILD_OK,
        );
    }

    /// The child half of the test above; a no-op outside its child.
    #[test]
    fn a_desktop_entry_with_no_exec_resolves_without_crashing_inner() {
        if std::env::var_os(NO_EXEC_CHILD).is_none() {
            return;
        }
        let mut cache = HashMap::new();
        let by_id = resolve_app_meta("ts-1434-no-exec", &mut cache);
        assert_eq!(
            by_id.as_ref().map(|m| m.display_name.as_str()),
            Some("TS 1434 No Exec"),
            "the entry is listed and resolves by its own id",
        );
        assert!(by_id.is_some_and(|m| m.icon.is_none()), "it has no Icon=");
        assert!(
            resolve_app_meta("totally-unrelated-app-id-for-1434", &mut cache).is_none(),
            "an entry with no Exec= has no executable stem to match, so layer 3 misses safely",
        );
        println!("{NO_EXEC_CHILD_OK}");
    }
}
