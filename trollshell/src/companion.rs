//! The `trollshell-control-center` companion app: where it lives and how to
//! start it, for the gear page's "Control Center" row (#1304).
//!
//! Two routes, resolved **once and before anything is clicked** — the same
//! rule the agent companion window's `Probe` states
//! (`crates/hytte-plugin-agents/src/window.rs`'s module doc): a detached
//! `systemd-run --user` launch reports success as soon as the user manager
//! takes the start job, so a program missing from `PATH` still answers
//! `ok: true` and only fails at exec, where nobody is listening
//! (`plugins::effects`' `start_detached`). A route decided at launch time
//! therefore cannot tell a missing binary from a real launch — deciding it up
//! front, and showing that decision on the row, is the only honest option.
//!
//! - **Desktop entry** (preferred): `nix/control-center.nix` installs
//!   [`DESKTOP_ID`]. Activating it through `gio::AppInfo::launch` also means a
//!   second click activates the *running* instance rather than starting a
//!   twin, because the control center is a unique-application-id
//!   `GApplication`.
//! - **Binary on `PATH`** (fallback — e.g. a `cargo run` dev session where the
//!   desktop entry was never installed): start [`BINARY`] detached through
//!   `launch.rs`'s `systemd-run --user` builder, the same mechanism
//!   `workspace_stacks::app_launch` and the plugin host's detached
//!   `RunCommand` (`plugins::effects::detached_launch`) already use, so it
//!   outlives `trollshell.service` the way a plugin does.
//! - **Neither**: [`Route::Missing`] — the row this feeds stays visible but
//!   insensitive, naming the binary, rather than a silent no-op.
//!
//! # Why `gio::AppInfo`, not `gio::DesktopAppInfo`
//!
//! `gio::DesktopAppInfo` is not available in the gio 0.22 bindings this
//! workspace vendors — `components/desktop_entry.rs`'s module doc records the
//! same constraint, and `components/app_meta.rs` / `widgets::calendar`'s
//! `launch_gnome_calendar` hit it too. So, like those, the desktop-entry route
//! is resolved by scanning `gio::AppInfo::all()` for the desktop id rather
//! than constructing a `GDesktopAppInfo` directly.

use std::path::{Path, PathBuf};

use hytte::gtk::{gdk, gio, prelude::*};

use crate::launch::{self, Launch};

/// The desktop entry `nix/control-center.nix` installs
/// (`makeDesktopItem { name = "mov.vibec0re.trollshell.ControlCenter"; }`).
pub(crate) const DESKTOP_ID: &str = "mov.vibec0re.trollshell.ControlCenter.desktop";

/// The binary name, resolved against `PATH` in the no-desktop-entry fallback.
pub(crate) const BINARY: &str = "trollshell-control-center";

/// How to start the control center — decided once, before any click.
#[derive(Clone, Debug)]
pub(crate) enum Route {
    /// The installed desktop entry. `gio::AppInfo::launch` either starts a
    /// fresh instance or, since the app is a unique-application-id
    /// `GApplication`, activates the one already running.
    DesktopEntry(gio::AppInfo),
    /// [`BINARY`] resolved to an absolute, executable path on `PATH`.
    Binary(PathBuf),
    /// Neither was found.
    Missing,
}

/// The pure route decision, over injected resolvers — the test seam.
///
/// `lookup` stands in for the `gio::AppInfo::all()` desktop-id scan, `path`
/// for the `PATH` search; each is handed the identifier it's resolving
/// ([`DESKTOP_ID`] / [`BINARY`]) so a test can assert what was actually asked
/// for. The desktop entry wins when both are present — see the module doc for
/// why a second click should focus the running instance rather than race a
/// detached launch against it.
pub(crate) fn resolve_control_center(
    lookup: impl Fn(&str) -> Option<gio::AppInfo>,
    path: impl Fn(&str) -> Option<PathBuf>,
) -> Route {
    if let Some(app_info) = lookup(DESKTOP_ID) {
        return Route::DesktopEntry(app_info);
    }
    if let Some(bin) = path(BINARY) {
        return Route::Binary(bin);
    }
    Route::Missing
}

/// [`resolve_control_center`] over the real desktop-entry scan and `$PATH`
/// search.
pub(crate) fn resolve() -> Route {
    resolve_control_center(lookup_desktop_entry, which)
}

/// [`gio::AppInfo::all`] scan for `id`, the same idiom
/// `components::desktop_entry::activate` and `widgets::calendar`'s
/// `launch_gnome_calendar` already use for the same `gio::DesktopAppInfo`
/// gap — see the module doc.
fn lookup_desktop_entry(id: &str) -> Option<gio::AppInfo> {
    gio::AppInfo::all()
        .into_iter()
        .find(|info| info.id().is_some_and(|found| found == id))
}

/// `name` resolved to an absolute path on `$PATH`: the first directory entry
/// that exists and has at least one execute bit — the same check
/// `hytte_plugin_agents::window`'s `Probe`/`is_executable` uses to answer
/// "would a launch find it", not `access(X_OK)` (this is not a security
/// decision).
fn which(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

/// An existing regular file with at least one execute bit.
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Act on a resolved [`Route`]. Cheap enough to call again on every click
/// (see [`resolve`]'s doc); [`Route::Missing`] does nothing — the row that
/// built it is insensitive, so this arm is unreachable through the UI, but a
/// no-op is still the correct answer if it's ever called anyway.
pub(crate) fn launch(route: &Route) {
    match route {
        Route::DesktopEntry(app_info) => {
            // A real `GdkAppLaunchContext`, not `gio::AppLaunchContext::NONE`:
            // this opens a window (unlike `launch_default_for_uri`'s "hand a
            // URI to whatever the desktop prefers"), and the display context
            // is what gives it correct startup notification / workspace
            // placement.
            let context = gdk::Display::default().map(|display| display.app_launch_context());
            if let Err(e) = app_info.launch(&[], context.as_ref()) {
                tracing::warn!(
                    error = %e,
                    "failed to launch the control center's desktop entry"
                );
            }
        }
        Route::Binary(path) => launch_binary(path),
        Route::Missing => {}
    }
}

/// The [`Route::Binary`] arm: a detached `systemd-run --user` launch, through
/// the shared builder in [`crate::launch`] — the same one
/// `workspace_stacks::app_launch` and the plugin host's detached `RunCommand`
/// use — so the control center outlives `trollshell.service` the way a
/// plugin, or a workspace-stack app, does.
///
/// No slice, no properties: this is a one-off windowed app the user asked
/// for from the gear page, not a supervised plugin or a workspace member with
/// a `Stop` transaction to land in — there is nothing here that needs to find
/// it again by cgroup.
fn launch_binary(path: &Path) {
    let launch = Launch {
        unit: "trollshell-control-center.service".to_owned(),
        description: "trollshell Control Center".to_owned(),
        // The display variables a windowed program needs, forwarded from
        // this shell the way a detached `RunCommand` and a workspace-stack
        // app already do (#953 L5 / #1071 §3.3): the user manager's own
        // `import-environment` (`etc/niri/session.kdl`) does not carry
        // `DISPLAY`, and a hand-started dev session's outer compositor is not
        // this one.
        env: forwarded_env(),
        argv: vec![path.display().to_string()],
        ..Launch::default()
    };
    let mut cmd = launch::command(launch::SYSTEMD_RUN, &launch);
    hytte::reactive::runtime::handle().spawn(async move {
        match cmd.status().await {
            Ok(status) if status.success() => {}
            Ok(status) => {
                tracing::warn!(%status, "systemd-run for the control center exited non-zero");
            }
            Err(e) => tracing::warn!(error = %e, "failed to spawn the control center"),
        }
    });
}

/// Display/IPC variables to forward to the detached launch, and their values
/// for the ones this shell actually has — the same set
/// `workspace_stacks::forwarded_env` forwards to a stack app (see that
/// function's doc for why the user manager's own environment import isn't
/// enough on its own).
fn forwarded_env() -> Vec<(String, String)> {
    ["WAYLAND_DISPLAY", "DISPLAY", "XDG_RUNTIME_DIR"]
        .into_iter()
        .filter_map(|name| {
            let value = std::env::var(name).ok()?;
            (!value.is_empty()).then(|| (name.to_owned(), value))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Route, resolve_control_center};
    use std::path::PathBuf;

    use hytte::gtk::gio;

    /// A `gio::AppInfo` a test can hand back as "the desktop entry was
    /// found", without needing a real installed `.desktop` file:
    /// `create_from_commandline` builds one entirely in memory — no D-Bus, no
    /// display, no `$XDG_DATA_DIRS` lookup — so this stays a hermetic `#[test]`
    /// rather than needing `#[gtk::test]`.
    fn fake_app_info() -> gio::AppInfo {
        gio::AppInfo::create_from_commandline(
            "true",
            Some("Control Center (test double)"),
            gio::AppInfoCreateFlags::NONE,
        )
        .expect("an in-memory AppInfo needs no desktop file, display or bus")
    }

    /// The desktop entry wins even when a binary is also on `PATH` — a
    /// second click should activate the running `GApplication`, not race a
    /// detached launch against it (see the module doc).
    ///
    /// **Falsification:** swap the two `if let` arms in
    /// `resolve_control_center` (binary checked first) → this reds, because
    /// `path` now answers first and `Route::Binary` comes back instead.
    #[test]
    fn the_desktop_entry_wins_over_the_binary() {
        let route = resolve_control_center(
            |id| {
                assert_eq!(id, super::DESKTOP_ID);
                Some(fake_app_info())
            },
            |name| {
                assert_eq!(name, super::BINARY);
                Some(PathBuf::from("/usr/bin/trollshell-control-center"))
            },
        );
        assert!(
            matches!(route, Route::DesktopEntry(_)),
            "the desktop entry must win when both routes are available"
        );
    }

    /// No desktop entry, but the binary is on `PATH` → [`Route::Binary`],
    /// carrying the resolved path.
    #[test]
    fn the_binary_is_used_when_there_is_no_desktop_entry() {
        let route = resolve_control_center(
            |_| None,
            |_| {
                Some(PathBuf::from(
                    "/home/user/.nix-profile/bin/trollshell-control-center",
                ))
            },
        );
        match route {
            Route::Binary(path) => assert_eq!(
                path,
                PathBuf::from("/home/user/.nix-profile/bin/trollshell-control-center")
            ),
            other => panic!("expected Route::Binary, got a different route ({other:?})"),
        }
    }

    /// Neither resolver finds anything → [`Route::Missing`].
    #[test]
    fn missing_when_neither_resolver_finds_anything() {
        let route = resolve_control_center(|_| None, |_| None);
        assert!(matches!(route, Route::Missing));
    }
}
