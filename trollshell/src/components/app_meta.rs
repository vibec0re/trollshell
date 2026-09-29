//! `app_meta` — resolve a Wayland/`cgroup` app-id to a desktop entry's display
//! name and icon.
//!
//! Extracted verbatim from `panels/stats.rs` (where it served the "Top apps"
//! expanders alone) when the Workspaces page (#1071 phase 1) became a second
//! consumer: a workspace card's stack row is a strip of app icons resolved from
//! the `app_id` niri reports for each window on that workspace. Two panels
//! resolving app-ids through two copies of these heuristics is exactly the
//! bug farm `components/` exists to prevent — same reasoning as
//! `reactive_list` (#174) and `hytte-config` one layer up (#640).
//!
//! Nothing about the lookup changed in the move; `stats.rs` imports it back.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use hytte::gtk::{gio, prelude::*};

/// Cached desktop app metadata resolved from `gio::AppInfo`.
///
/// Note: `gio::DesktopAppInfo` is not available in gio 0.22 bindings, so we
/// use the `gio::AppInfo` interface (the abstract interface) via
/// `gio::AppInfo::all()`, which returns all installed applications with their
/// ids, display names, and icons. We scan this list lazily (once per new
/// app-id) and cache the result for the lifetime of the expander widget.
#[derive(Clone)]
pub(crate) struct AppMeta {
    pub(crate) display_name: String,
    pub(crate) icon: Option<gio::Icon>,
}

/// The caller-owned cache [`resolve_app_meta`] fills: app-id → resolution,
/// with `None` meaning "scanned, no desktop entry" so a miss also costs at
/// most one `AppInfo::all()` scan.
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

/// Resolve display name and icon for an app-id via a layered `gio::AppInfo`
/// lookup, caching the result so the scan happens at most once per unique
/// app-id per expander lifetime.
///
/// A cache hit costs no scan at all — the cache is checked before
/// [`gio::AppInfo::all`] is ever called, which is also why this stays a thin
/// wrapper over [`resolve_in`] rather than folding the cache check into it:
/// `resolve_in` takes the scanned list as a plain argument, so pushing the
/// caching down a level would force the scan to run on every call (Rust
/// evaluates a call's arguments before the call), even on a hit.
///
/// See [`resolve_in`] for the three lookup layers themselves.
pub(crate) fn resolve_app_meta(
    app_id: &str,
    meta_cache: &mut HashMap<String, Option<AppMeta>>,
) -> Option<AppMeta> {
    if let Some(cached) = meta_cache.get(app_id) {
        return cached.clone();
    }
    let meta = resolve_in(&gio::AppInfo::all(), app_id);
    meta_cache.insert(app_id.to_string(), meta.clone());
    meta
}

/// [`resolve_app_meta`]'s three lookup layers, over a caller-supplied
/// `AppInfo` list rather than a fresh [`gio::AppInfo::all`] scan — the seam
/// that lets a test drive them over a hand-picked list (#1434's regression
/// test is the reason this was split out; see its module-`tests` doc for why
/// the list still has to come from a real scan rather than an in-memory
/// fixture).
///
/// Tries the following strategies in order, stopping at the first hit:
///
/// 1. **Exact id match** — an `all` entry whose id equals `<app_id>.desktop`
///    (or its lowercase variant). Fast for well-behaved desktop files.
/// 2. **Case-insensitive id containment** — scans for an entry whose desktop
///    file id (without the `.desktop` suffix) case-insensitively contains, or
///    is contained by, the app-id. Catches reverse-DNS mismatches such as
///    `org.gnome.Nautilus.desktop` for app-id `org.gnome.Nautilus`, and
///    NixOS wrapper names like `firefox-unwrapped` matching `firefox.desktop`.
/// 3. **Executable basename match** — entry whose executable file stem
///    case-insensitively equals the app-id. Catches cgroup scope leaves like
///    `app-firefox.scope`→`firefox` when the desktop file is `Firefox.desktop`
///    with executable `/usr/bin/firefox`.
///
/// Note: `gio::DesktopAppInfo::search` and `startup_wm_class` are not
/// available in the gio 0.22 bindings used here, so the above heuristics
/// approximate their behaviour using the `AppInfo` abstract interface.
pub(crate) fn resolve_in(all: &[gio::AppInfo], app_id: &str) -> Option<AppMeta> {
    let app_id_lower = app_id.to_lowercase();

    // Layer 1: exact id match (fast path).
    let exact = format!("{app_id}.desktop");
    let exact_lower = format!("{app_id_lower}.desktop");
    let hit = all.iter().find(|info| {
        info.id()
            .is_some_and(|id| id == exact.as_str() || id == exact_lower.as_str())
    });

    // Layer 2: case-insensitive id containment.
    // Strips the `.desktop` suffix and checks if the stem contains the app-id
    // or vice-versa (handles reverse-DNS and wrapper-name mismatches).
    let hit = hit.or_else(|| {
        all.iter().find(|info| {
            info.id().is_some_and(|id| {
                let stem = id
                    .as_str()
                    .strip_suffix(".desktop")
                    .unwrap_or(id.as_str())
                    .to_lowercase();
                stem.contains(app_id_lower.as_str())
                    || app_id_lower.as_str().contains(stem.as_str())
            })
        })
    });

    // Layer 3: executable basename match.
    // Catches cases where the desktop file uses a different name but the
    // binary matches (e.g. `firefox` binary → `Firefox.desktop`).
    let hit = hit.or_else(|| {
        all.iter().find(|info| {
            // #1434: `g_app_info_get_executable()` isn't annotated
            // `(nullable)`, so gio-rs binds `executable()` as a plain
            // `PathBuf` — but a `DBusActivatable=true` desktop entry may have
            // no `Exec=` line at all and GLib still loads it, and its
            // executable is then genuinely `NULL`. Calling `executable()`
            // unconditionally aborts a debug build (glib-rs's
            // `debug_assert!`) and segfaults a release build
            // (`CStr::from_ptr(NULL)`). `commandline()` reads the same
            // underlying field (`binary`) but *is* bound `Option`-typed and
            // is non-`None` exactly when `executable()` would be non-`NULL`,
            // so checking it first skips exactly the entries that would
            // otherwise crash.
            if info.commandline().is_none() {
                return false;
            }
            let exe = info.executable();
            exe.file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|stem| stem.to_lowercase() == app_id_lower.as_str())
        })
    });

    // `display_name()` is the file's other non-`Option` `gio::AppInfo`
    // accessor, but it's safe: `g_desktop_app_info_get_display_name` falls
    // back to `g_desktop_app_info_get_name`, which itself falls back to the
    // literal string `"Unnamed"` when the entry has no `Name=` key — so it
    // never returns `NULL`, unlike `executable()` above. `icon()` is already
    // `Option`-typed in gio-rs and handled as such.
    hit.map(|info| AppMeta {
        display_name: info.display_name().to_string(),
        icon: info.icon(),
    })
}

/// The icon shown for an app-id that resolves to no desktop entry (or to an
/// entry carrying no icon of its own).
pub(crate) fn fallback_icon() -> gio::Icon {
    gio::ThemedIcon::new("application-x-executable-symbolic").upcast::<gio::Icon>()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;

    use super::{resolve_app_meta, resolve_in};
    use hytte::gtk::{gio, prelude::*};

    /// #1434 needs a real `gio::AppInfo` whose `commandline()`/`executable()`
    /// is `NULL` — a `DBusActivatable=true` desktop entry with no `Exec=`
    /// line. `gio::AppInfo::create_from_commandline` (the in-memory
    /// constructor `companion.rs`'s tests use) always takes a commandline, so
    /// it cannot produce that shape; and gio-rs 0.22 has no `DesktopAppInfo`
    /// binding to build one from a keyfile directly (this file's own doc, and
    /// `components::desktop_entry`'s). The only thing that *can* produce one
    /// is GIO's own desktop-file scan (`gio::AppInfo::all()`), which reads
    /// `$XDG_DATA_HOME`/`$XDG_DATA_DIRS` — and `std::env::set_var` is
    /// `unsafe` in edition 2024 (this workspace forbids `unsafe_code`
    /// outright), so no in-process test can point that scan at a fixture
    /// directory for itself.
    ///
    /// Same fix as the established precedents for this exact constraint
    /// (`hytte-plugin-stats::plugin::tests::settings_reads_the_real_process_environment`,
    /// `trollshell::plugins::tests::detached_launch_falls_back_without_a_user_manager`):
    /// re-exec this test binary (`std::env::current_exe`), filtered to
    /// exactly one inner test, with the fixture directories set on the
    /// **child** via the safe `Command::env` builder — a brand-new process's
    /// environment is set before it starts, which needs no `unsafe`.
    ///
    /// No display is involved anywhere in this — `gio::AppInfo::all()` is
    /// plain GIO, not GTK — so both tests below stay hermetic `#[test]`s
    /// rather than `system-tests`-gated.
    fn write_desktop_entry(xdg_data_home: &Path, file_name: &str, contents: &str) {
        let apps_dir = xdg_data_home.join("applications");
        std::fs::create_dir_all(&apps_dir).expect("create the fixture applications/ directory");
        std::fs::write(apps_dir.join(file_name), contents)
            .expect("write the fixture .desktop file");
    }

    const NO_EXEC_CHILD: &str = "TROLLSHELL_APP_META_1434_NO_EXEC_CHILD";
    const NO_EXEC_CHILD_OK: &str = "app-meta-1434-no-exec-child-reached-the-end";

    /// The crash regression itself: layer 3 used to call `executable()`
    /// unconditionally inside `.find()`'s predicate, which aborts a debug
    /// build and segfaults a release build when the entry has no `Exec=`
    /// (see `resolve_in`'s layer-3 comment). With the guard, this must
    /// instead just miss.
    ///
    /// **Falsification:** delete the `if info.commandline().is_none() {
    /// return false; }` guard in `resolve_in`'s layer 3 — the child then
    /// aborts inside glib-rs's `debug_assert!` instead of printing
    /// [`NO_EXEC_CHILD_OK`], and this test reds (verified by hand while
    /// building this fix, then restored).
    #[test]
    fn layer_three_skips_a_desktop_entry_with_no_exec_without_crashing() {
        let inner = "components::app_meta::tests::\
                      layer_three_skips_a_desktop_entry_with_no_exec_without_crashing_inner";
        let exe = std::env::current_exe().expect("this test binary's own path");
        let data_home = tempfile::tempdir().expect("a scratch XDG_DATA_HOME");
        let data_dirs = tempfile::tempdir().expect("a scratch, empty XDG_DATA_DIRS");
        write_desktop_entry(
            data_home.path(),
            "ts-1434-no-exec.desktop",
            "[Desktop Entry]\nType=Application\nName=TS 1434 No Exec\nDBusActivatable=true\n",
        );

        let out = std::process::Command::new(&exe)
            .args(["--exact", "--nocapture", "--test-threads=1", inner])
            .env(NO_EXEC_CHILD, "1")
            .env("XDG_DATA_HOME", data_home.path())
            .env("XDG_DATA_DIRS", data_dirs.path())
            .output()
            .expect("re-exec this test binary with a fixture XDG_DATA_HOME");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "the child must skip the no-Exec entry, not crash ({:?})\n\
             --- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
            out.status,
        );
        assert!(
            stdout.contains(NO_EXEC_CHILD_OK),
            "the child exited 0 without reaching the end of {inner} — a stale filter matches \
             no test and libtest still reports success\n\
             --- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        );
    }

    /// The child half of the test above. A no-op unless the parent's marker
    /// is set, so an ordinary `cargo test` run — which discovers it like any
    /// other test — doesn't try to run the scenario with no fixture directory
    /// set up.
    #[test]
    fn layer_three_skips_a_desktop_entry_with_no_exec_without_crashing_inner() {
        if std::env::var_os(NO_EXEC_CHILD).is_none() {
            return;
        }
        let all = gio::AppInfo::all();
        let fixture: Vec<gio::AppInfo> = all
            .into_iter()
            .filter(|info| info.id().is_some_and(|id| id == "ts-1434-no-exec.desktop"))
            .collect();
        assert_eq!(
            fixture.len(),
            1,
            "test setup: GIO must have picked up exactly the one fixture entry"
        );

        // An id that cannot match layer 1 (exact) or layer 2 (containment)
        // against the fixture's own id/stem, so this reaches layer 3 — the
        // layer that used to crash — for the one entry in `fixture`.
        let meta = resolve_in(&fixture, "totally-unrelated-app-id-for-1434");
        assert!(
            meta.is_none(),
            "an entry with no Exec= has no executable stem to match, so this must miss safely"
        );
        println!("{NO_EXEC_CHILD_OK}");
    }

    const VALID_CHILD: &str = "TROLLSHELL_APP_META_1434_VALID_CHILD";
    const VALID_CHILD_OK: &str = "app-meta-1434-valid-child-reached-the-end";

    /// `resolve_app_meta` — the public wrapper, not [`resolve_in`] — must
    /// still call the seam with a real [`gio::AppInfo::all`] scan; nothing
    /// upstream of this file's own tests would otherwise notice a wrapper
    /// whose call site stopped doing that.
    ///
    /// **Falsification (verified by hand, then reverted):** change
    /// `resolve_app_meta`'s call from `resolve_in(&gio::AppInfo::all(), …)`
    /// to `resolve_in(&[], …)` — the child then finds nothing for the
    /// fixture id and this test reds, where
    /// [`layer_three_skips_a_desktop_entry_with_no_exec_without_crashing`]
    /// (which drives `resolve_in` directly) does not move at all.
    #[test]
    fn resolve_app_meta_resolves_a_desktop_entry_through_app_info_all() {
        let inner = "components::app_meta::tests::\
                      resolve_app_meta_resolves_a_desktop_entry_through_app_info_all_inner";
        let exe = std::env::current_exe().expect("this test binary's own path");
        let data_home = tempfile::tempdir().expect("a scratch XDG_DATA_HOME");
        let data_dirs = tempfile::tempdir().expect("a scratch, empty XDG_DATA_DIRS");
        write_desktop_entry(
            data_home.path(),
            "ts-1434-valid.desktop",
            // A bare, `$PATH`-relative `Exec=` — not an absolute path — since
            // NixOS has no `/usr/bin` and GIO excludes an entry from
            // `AppInfo::all()`'s results outright when an absolute `Exec=`
            // target doesn't exist on disk (measured: `/usr/bin/true` here
            // made this fixture invisible to the very scan under test).
            "[Desktop Entry]\nType=Application\nName=TS 1434 Valid\nExec=true\n",
        );

        let out = std::process::Command::new(&exe)
            .args(["--exact", "--nocapture", "--test-threads=1", inner])
            .env(VALID_CHILD, "1")
            .env("XDG_DATA_HOME", data_home.path())
            .env("XDG_DATA_DIRS", data_dirs.path())
            .output()
            .expect("re-exec this test binary with a fixture XDG_DATA_HOME");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "the child must resolve the fixture entry ({:?})\n\
             --- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
            out.status,
        );
        assert!(
            stdout.contains(VALID_CHILD_OK),
            "the child exited 0 without reaching the end of {inner} — a stale filter matches \
             no test and libtest still reports success\n\
             --- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        );
    }

    /// The child half of the test above.
    #[test]
    fn resolve_app_meta_resolves_a_desktop_entry_through_app_info_all_inner() {
        if std::env::var_os(VALID_CHILD).is_none() {
            return;
        }
        let mut cache = HashMap::new();
        let meta = resolve_app_meta("ts-1434-valid", &mut cache);
        let meta = meta.expect("resolve_app_meta must find the fixture through AppInfo::all()");
        assert_eq!(meta.display_name, "TS 1434 Valid");
        println!("{VALID_CHILD_OK}");
    }
}
