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
/// lookup.
///
/// Tries the following strategies in order, stopping at the first hit:
///
/// 1. **Exact id match** — `AppInfo::all()` entry whose id equals
///    `<app_id>.desktop` (or its lowercase variant). Fast for well-behaved
///    desktop files.
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
/// All three layers scan `AppInfo::all()` (or share the same pre-fetched list)
/// and cache the result so the work happens at most once per unique app-id per
/// expander lifetime.
///
/// Note: `gio::DesktopAppInfo::search` and `startup_wm_class` are not
/// available in the gio 0.22 bindings used here, so the above heuristics
/// approximate their behaviour using the `AppInfo` abstract interface.
pub(crate) fn resolve_app_meta(
    app_id: &str,
    meta_cache: &mut HashMap<String, Option<AppMeta>>,
) -> Option<AppMeta> {
    if let Some(cached) = meta_cache.get(app_id) {
        return cached.clone();
    }

    let app_id_lower = app_id.to_lowercase();

    // Fetch all installed apps once for this lookup.
    let all = gio::AppInfo::all();

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
            let exe = info.executable();
            exe.file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|stem| stem.to_lowercase() == app_id_lower.as_str())
        })
    });

    let meta = hit.map(|info| AppMeta {
        display_name: info.display_name().to_string(),
        icon: info.icon(),
    });
    meta_cache.insert(app_id.to_string(), meta.clone());
    meta
}

/// The icon shown for an app-id that resolves to no desktop entry (or to an
/// entry carrying no icon of its own).
pub(crate) fn fallback_icon() -> gio::Icon {
    gio::ThemedIcon::new("application-x-executable-symbolic").upcast::<gio::Icon>()
}
