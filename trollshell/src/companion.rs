//! The `trollshell-control-center` companion app: where it lives and how to
//! start it, for the gear page's "Control Center" row (#1304).
//!
//! Two routes, resolved **before anything is clicked** — the same rule the
//! agent companion window's `Probe` states
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
//!   outlives `trollshell.service` the way a plugin does. Each launch gets a
//!   fresh `--unit=` (via `plugins::effects::allocate_launch_unit`, the same
//!   pid+sequence uniquifier a detached `RunCommand` uses) rather than one
//!   fixed name — a fixed name made a second fallback launch a silent no-op
//!   for exactly as long as the control center was already open (#1305
//!   review MED-2: `systemd-run` refuses a unit name still in use with "was
//!   already loaded or has a fragment file", and `--collect` only frees a
//!   name once the program *exits*).
//! - **Neither**: [`Route::Missing`] — the row this feeds stays visible but
//!   insensitive, naming the binary, rather than a silent no-op.
//!
//! Routes are re-resolved on demand, not cached across the session
//! ([`resolve`] is called both when the gear page's row is built and on every
//! `open-control-center` action activation, `commands.rs`) — installing the
//! control center *while the shell is running* (a `nixos-rebuild switch`, a
//! home-manager generation, `nix profile install`) makes both routes appear
//! with no shell restart needed on the resolver's side. The row itself does
//! **not** notice, though: `modal::ensure_page` caches the whole Settings page
//! for the shell's lifetime, so a row built while the binary was missing stays
//! insensitive until the shell restarts, even though `open-control-center`
//! (which resolves fresh every activation) starts working immediately. See
//! `panels::settings::control_center_row`'s doc for that asymmetry stated
//! plainly, rather than "neither route can appear or disappear without a
//! restart" — which is false: a new install changes what both `resolve()`
//! calls above would return, it's specifically the *cached row* that misses
//! it.
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
use crate::plugins::effects;

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
/// search — but `which(BINARY)` runs **first**, and [`lookup_desktop_entry`]'s
/// `gio::AppInfo::all()` scan is skipped entirely when it finds nothing.
///
/// That reordering is a measured optimisation, not a behaviour change: GIO
/// refuses to construct — and so never lists via `AppInfo::all()` — a
/// `GDesktopAppInfo` whose `Exec=` program does not resolve on `PATH`
/// (`workspace_stacks.rs` documents the identical refusal for `TryExec`), and
/// [`DESKTOP_ID`]'s entry names [`BINARY`] verbatim as its `Exec=`
/// (`nix/control-center.nix`). So when the binary is missing, the scan is
/// **provably** going to answer `None` — `DesktopEntry` practically implies
/// `Binary`, confirmed by construction (401 synthetic entries + the real
/// `mov.vibec0re.trollshell.ControlCenter.desktop`): with the binary off
/// `PATH`, `AppInfo::all()` never lists it (`resolve()` → `Missing`); adding a
/// dummy `trollshell-control-center` to `PATH` with the same directories
/// unchanged is what makes it appear.
///
/// Skipping the scan matters because it isn't free: measured on this box,
/// `g_app_info_get_all()` costs a mean 566µs at 7 installed entries and 14.4ms
/// at 407 (13.6ms of that *inside* the C call, so `--release` doesn't move
/// it). Neither call site is a tick — `panels::settings::control_center_row`
/// runs this once per cached page, `commands.rs`'s `open-control-center`
/// action once per activation — so this is a dropped frame on an occasional
/// keybind press or page build, not a stutter, and this reordering removes it
/// entirely on the common "not installed" desktop.
pub(crate) fn resolve() -> Route {
    let Some(bin) = which(BINARY) else {
        return Route::Missing;
    };
    resolve_control_center(lookup_desktop_entry, move |_| Some(bin.clone()))
}

/// [`gio::AppInfo::all`] scan for `id`, the same idiom
/// `components::desktop_entry::activate` and `widgets::calendar`'s
/// `launch_gnome_calendar` already use for the same `gio::DesktopAppInfo`
/// gap — see the module doc. [`resolve`]'s doc records why this scan is
/// skipped whenever [`which`] already answered `None` for [`BINARY`].
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

/// Act on a resolved [`Route`]. [`Route::Missing`] does nothing — the row that
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

/// The [`Route::Binary`] arm's [`Launch`], pure and independently testable
/// (the shape `workspace_stacks::app_launch` already has, and the one this
/// path was missing per #1305 review MED-3 — mutating [`launch_binary`]'s
/// body to an early `return` used to leave every gate green).
///
/// No slice, no properties: this is a one-off windowed app the user asked
/// for from the gear page, not a supervised plugin or a workspace member with
/// a `Stop` transaction to land in — there is nothing here that needs to find
/// it again by cgroup. The unit name comes from
/// [`effects::allocate_launch_unit`] (a pid+sequence suffix over
/// `"control-center"`) rather than a fixed string, so a second launch while
/// the first is still running gets its own unit instead of colliding with it
/// (#1305 review MED-2).
fn control_center_launch(path: &Path) -> Launch {
    Launch {
        unit: effects::allocate_launch_unit("control-center", 0),
        description: "trollshell Control Center".to_owned(),
        // The display variables a windowed program needs, forwarded from
        // this shell — the *same* list `workspace_stacks::forwarded_env`
        // forwards to a stack app (that function's doc: user-manager
        // `import-environment` doesn't carry `NIRI_SOCKET`/`DISPLAY`), reused
        // rather than a second copy of it (#1305 review MED-4).
        env: crate::workspace_stacks::forwarded_env(),
        argv: vec![path.display().to_string()],
        ..Launch::default()
    }
}

/// The [`Route::Binary`] arm: runs [`control_center_launch`]'s `systemd-run
/// --user` invocation through [`crate::launch`]'s shared builder — the same
/// one `workspace_stacks::app_launch` and the plugin host's detached
/// `RunCommand` use — so the control center outlives `trollshell.service` the
/// way a plugin, or a workspace-stack app, does.
fn launch_binary(path: &Path) {
    let launch = control_center_launch(path);
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

#[cfg(test)]
mod tests {
    use super::{Route, control_center_launch, resolve_control_center};
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

    /// #1305 review MED-2 + MED-3: the `Route::Binary` launch's exact argv
    /// shape, pinned the way the two pre-existing `Launch` call sites already
    /// are (`plugin_launcher::systemd_run_args_pin_the_invocation`,
    /// `plugins::tests::detached_launch_wraps_the_argv_in_a_systemd_run_service_unit`)
    /// — over `launch::argv_of`/`command`, the assertion seam those two use,
    /// rather than a third hand-rolled one.
    ///
    /// The env portion is asserted against `workspace_stacks::forwarded_env()`
    /// itself, called a second time right here, rather than a literal
    /// `WAYLAND_DISPLAY=…` — this crate has no `temp_env`-shaped dependency to
    /// pin the *process* environment without `unsafe` (edition 2024 makes
    /// `std::env::set_var` exactly that; `plugins::effects`' own
    /// `filter_forwarded_env` split exists for the identical reason), and a
    /// literal would either assert nothing true in a sandbox with none of
    /// these four set, or assert a value this box happens to have. Comparing
    /// against the shared function's own live output instead still pins the
    /// property that actually matters here (#1305 review MED-4): this route
    /// embeds exactly what `workspace_stacks::forwarded_env()` returns, in its
    /// order — not a second, independently drifting list.
    ///
    /// **Falsification:** revert `control_center_launch`'s `unit` field to
    /// the old fixed `"trollshell-control-center.service"` → the
    /// `starts_with` assertion on the unit prefix still passes (it's a
    /// substring check) but the **second** launch's unit no longer differs
    /// from the first's, which the "two launches never collide" assertion
    /// below catches; replacing `control_center_launch`'s body with an early
    /// `return Launch::default()` reds every assertion in this test at once.
    #[test]
    fn the_binary_route_launch_pins_its_argv_and_never_reuses_a_unit_name() {
        let path = std::path::Path::new("/usr/bin/trollshell-control-center");
        let first = control_center_launch(path);
        let second = control_center_launch(path);

        assert_ne!(
            first.unit, second.unit,
            "two launches must never share a unit name, or the second is a silent \
             no-op for as long as the first is still running (#1305 review MED-2)"
        );

        let cmd = crate::launch::command(crate::launch::SYSTEMD_RUN, &first);
        let args = crate::launch::argv_of(&cmd);

        assert_eq!(&args[..3], ["--user", "--quiet", "--collect"]);
        let unit_arg = args
            .iter()
            .find(|a| a.starts_with("--unit="))
            .expect("a --unit= flag is always emitted");
        assert!(
            unit_arg.starts_with("--unit=trollshell-launch-control-center-0-"),
            "unexpected unit shape: {unit_arg}"
        );
        assert!(unit_arg.ends_with(".service"), "{unit_arg}");
        assert!(
            args.contains(&"--description=trollshell Control Center".to_owned()),
            "{args:?}"
        );
        assert!(
            !args.iter().any(|a| a.starts_with("--slice=")),
            "a one-off windowed launch carries no slice: {args:?}"
        );
        assert!(
            !args.iter().any(|a| a.starts_with("--property=")),
            "a one-off windowed launch carries no properties: {args:?}"
        );

        let sep = args
            .iter()
            .position(|a| a == "--")
            .expect("separator present");
        let expected_env: Vec<String> = crate::workspace_stacks::forwarded_env()
            .into_iter()
            .map(|(k, v)| format!("--setenv={k}={v}"))
            .collect();
        let argv_env: Vec<String> = args[..sep]
            .iter()
            .filter(|a| a.starts_with("--setenv="))
            .cloned()
            .collect();
        assert_eq!(
            argv_env, expected_env,
            "the forwarded env must be exactly workspace_stacks::forwarded_env()'s \
             own output, in order — not a second, independently drifting list"
        );

        assert_eq!(
            &args[sep + 1..],
            &["/usr/bin/trollshell-control-center".to_owned()],
            "the resolved binary path is the whole argv after --"
        );
    }
}
