//! Light/Dark theme switching for the running session.
//!
//! Trollshell-style shells are the compositor session, so a "set theme"
//! action has to fan out to every toolkit family the user has running:
//!
//! 1. **GTK4 / libadwaita** — `org.gnome.desktop.interface color-scheme`
//!    (`prefer-light` / `prefer-dark`). Apps using `adw::StyleManager` honor
//!    this live via gsettings.
//! 2. **Legacy GTK (2/3) and non-libadwaita GTK4** — `gtk-theme` gsettings
//!    key (`Adwaita` / `Adwaita-dark`) plus `~/.config/gtk-{3,4}.0/settings.ini`
//!    fallbacks for apps that don't go through xsettings/dconf.
//! 3. **Qt** — `~/.config/qt[56]ct/qt[56]ct.conf [Appearance]` keys
//!    `style`, `custom_palette`, `color_scheme_path`. Sets `style=Fusion`
//!    (Qt built-in, always present) and toggles a dark palette via
//!    qt[56]ct's bundled `darker.conf`. Effective when `qt[56]ct` is
//!    installed and `QT_QPA_PLATFORMTHEME=qt[56]ct` is exported. The conf
//!    is written unconditionally; with no qt[56]ct platform theme loaded
//!    it costs nothing.
//!
//! Every subprocess and every ini-file write runs on the `hytte_reactive`
//! tokio runtime, never on the GTK main thread (#1171) — `set()` only
//! updates the in-memory current-theme handle synchronously (a cheap
//! `Mutable::set`, no I/O) before handing the actual fan-out to
//! [`runtime::handle()`]. `tokio::process::Command::status()`/`::output()`
//! await the child to completion, so unlike the old fire-and-forget
//! `std::process::Command::spawn()` (never `.wait()`ed) this can't leak a
//! zombie per gsettings call either. The four ini/conf rewrites are
//! synchronous `std::fs` read-modify-writes under `$HOME`, so they go
//! through `spawn_blocking` rather than sitting on an async worker — the
//! same move `brightness`'s sysfs walk makes, for the same reason (#1192
//! review, LOW-2). Failures on any one fan-out target are logged and the
//! others still run — best-effort, because partial coverage is strictly
//! better than aborting the whole switch on (e.g.) a missing qt6ct dir.
//!
//! The current theme is **reactive, not a snapshot**: [`current_signal`] is
//! the accessor a UI binds, and it carries an `Option<Theme>` whose `None`
//! means "the seed read hasn't landed yet" — `Loading` in the
//! `hytte_bus::PropState` sense. There is deliberately no theme-shaped
//! fallback value in that state: a switch bound to this is *insensitive*
//! while the answer is unknown rather than confidently wrong, which is the
//! regression #1192's review caught (a light session read `Dark`
//! deterministically, because the call that read the value was the same
//! call that started the read).

use futures_signals::signal::{Mutable, Signal};
use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use hytte_reactive::runtime;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Theme {
    Light,
    Dark,
}

impl Theme {
    fn color_scheme(self) -> &'static str {
        match self {
            Theme::Light => "prefer-light",
            Theme::Dark => "prefer-dark",
        }
    }

    fn gtk_theme(self) -> &'static str {
        match self {
            Theme::Light => "Adwaita",
            Theme::Dark => "Adwaita-dark",
        }
    }

    fn is_dark(self) -> bool {
        matches!(self, Theme::Dark)
    }
}

// ── Cross-thread current-theme handle ──────────────────────────────────────
//
// `hytte_reactive::registry` is thread-local to the GTK main thread; `set()`
// used to run its whole body (including a blocking subprocess call) on
// whatever thread called it — the GTK thread for every caller today
// (`panels/settings.rs`). Fixing that means the fan-out has to move to the
// tokio runtime, which rules out the registry as the home for the
// current-theme value. A process-global `Mutable` (the same shape
// `brightness.rs`'s `DEVICE` uses for its write-target device) is
// cross-thread-safe and needs no `Service`/`App::with` registration, so
// `current()`/`set()` keep working exactly as free functions.
//
// The payload is `Option<Theme>`, **not** `Theme`: see `current_handle`.
static CURRENT: OnceLock<Mutable<Option<Theme>>> = OnceLock::new();

/// The shared current-theme handle. Seeded exactly once, the first time
/// anything asks for it, by spawning an async `gsettings get` on the tokio
/// runtime — never a blocking read on the caller's thread.
///
/// `None` is "not read yet", and it is load-bearing rather than a stylistic
/// `Option`. This used to be a `Mutable<Theme>` initialised to
/// `Theme::Dark`, which made the *first* read of [`current`] report Dark
/// **deterministically** — not as a race, but because the call that reads
/// the value is the same call that starts the read. The single consumer
/// (`trollshell`'s Settings panel) read it once at page build and cached
/// the page, so a light-mode session got a "Dark mode: on" switch for the
/// life of the shell (#1192 review, HIGH-1). Keeping "unknown" unspellable
/// as a `Theme` is what makes that bug unrepresentable: a consumer must
/// decide what to do with `None`, and the honest answer (an insensitive
/// switch until the seed lands) is the one `panels/settings.rs` takes.
///
/// `set()` keeps this handle in sync going forward without re-querying
/// gsettings on the happy path — and corrects it from a real re-read when
/// the write turns out to have failed (see [`do_set`]).
fn current_handle() -> &'static Mutable<Option<Theme>> {
    CURRENT.get_or_init(|| {
        let mutable = unseeded_handle();
        spawn_seed(&mutable, read_current);
        mutable
    })
}

/// A fresh, *unseeded* current-theme handle. One line, and split out on
/// purpose: this is the production initial value, so the HIGH-1 regression
/// test can build a handle that is `None`-at-birth for the same reason the
/// real one is, instead of asserting against a `None` it wrote itself. Put
/// `Some(Theme::Dark)` back here — the pre-fix shape — and
/// `a_light_session_is_never_observed_as_dark` goes red.
fn unseeded_handle() -> Mutable<Option<Theme>> {
    Mutable::new(None)
}

/// Current theme as last known by this process, or `None` while the seed
/// read is still in flight. Never blocks and never spawns a subprocess
/// itself — see [`current_handle`] for how the value is seeded and kept in
/// sync. On any `gsettings` error, or a `default` value (externally set,
/// "follow system" — trollshell sessions don't have a system to follow),
/// the seed read resolves to `Theme::Dark`, matching
/// `adw::ColorScheme::PreferDark` defaults.
///
/// Prefer [`current_signal`] in a UI: a one-shot read taken at widget-build
/// time is almost always `None`, and nothing re-reads it afterwards.
#[must_use]
pub fn current() -> Option<Theme> {
    current_handle().get()
}

/// Reactive view of [`current`] — the accessor a UI binds.
///
/// Emits `None` immediately (seed in flight), then `Some(theme)` when the
/// `gsettings get` lands, and again on every [`set`] (including a
/// correction when the write failed). `bind_two_way` is the right seam on
/// the consuming side: a plain `bind` would drive the switch's
/// `active` property programmatically, whose `notify` handler calls back
/// into [`set`] and fans a whole `gsettings` write out at startup for a
/// value nobody changed.
pub fn current_signal() -> impl Signal<Item = Option<Theme>> {
    current_handle().signal()
}

/// Spawn the one-shot seed read that fills `handle`, exactly the way
/// [`current_handle`] does. Split out (and parameterized on the reader) so
/// the seed *mechanism* — "subscribers see `None` first, then the value the
/// session actually reports, and never a wrong theme in between" — is
/// testable against a fake `gsettings` without touching the process's `PATH`
/// or the process-global handle (#1192 review, HIGH-1).
fn spawn_seed<F, Fut>(handle: &Mutable<Option<Theme>>, read: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Theme> + Send + 'static,
{
    let writer = handle.clone();
    runtime::handle().spawn(async move {
        writer.set(Some(read().await));
    });
}

/// Async `gsettings get org.gnome.desktop.interface color-scheme`, run on
/// the tokio runtime. Never called from the GTK thread directly — only via
/// [`current_handle`]'s seed spawn.
///
/// **Main-thread guarantee is by construction, not by test:** this `fn` is
/// `async` and only ever reached through `runtime::handle().spawn(...)`
/// (here and in [`set`]/[`do_set`]) — there is no synchronous call path from
/// `current()`/`set()` into a `Command` at all, so nothing short of a new
/// call site added directly into `current()` or `set()` could reintroduce a
/// blocking subprocess call on whatever thread calls them.
async fn read_current() -> Theme {
    read_current_from("gsettings").await
}

/// [`read_current`] with the program spelled out, so a test can point it at
/// a fake `gsettings` on an absolute path instead of mutating `PATH`
/// (`std::env::set_var` is `unsafe`, and this crate `forbid`s unsafe).
/// Production always passes `"gsettings"`.
async fn read_current_from(program: &str) -> Theme {
    let output = tokio::process::Command::new(program)
        .args(["get", "org.gnome.desktop.interface", "color-scheme"])
        .output()
        .await;
    match output {
        Ok(out) if out.status.success() => {
            // gsettings get prints quoted strings, e.g. `'prefer-dark'\n`.
            let raw = String::from_utf8_lossy(&out.stdout);
            let trimmed = raw.trim().trim_matches('\'').trim_matches('"');
            match trimmed {
                "prefer-light" => Theme::Light,
                _ => Theme::Dark,
            }
        }
        Ok(out) => {
            tracing::warn!(
                stderr = %String::from_utf8_lossy(&out.stderr),
                "theme: gsettings get color-scheme failed",
            );
            Theme::Dark
        }
        Err(e) => {
            tracing::warn!(error = %e, "theme: gsettings unavailable");
            Theme::Dark
        }
    }
}

/// Apply `theme` across every toolkit family. See module docs for the
/// fan-out targets and the rationale for best-effort failure handling.
///
/// Updates the cross-thread current-theme handle synchronously (cheap, no
/// I/O) before returning, so a caller that immediately re-reads [`current`]
/// sees the new value even though the actual fan-out is still running on the
/// tokio runtime. That optimistic update is **corrected** if the
/// authoritative `color-scheme` write turns out to have failed — see
/// [`do_set`].
pub fn set(theme: Theme) {
    current_handle().set(Some(theme));
    runtime::handle().spawn(async move {
        do_set(theme).await;
    });
}

/// The actual fan-out, run entirely on the tokio runtime (#1171) — every
/// subprocess spawn and every ini-file write happens here, never on the GTK
/// main thread.
async fn do_set(theme: Theme) {
    let color_scheme_ok = run_gsettings(&[
        "set",
        "org.gnome.desktop.interface",
        "color-scheme",
        theme.color_scheme(),
    ])
    .await;
    run_gsettings(&[
        "set",
        "org.gnome.desktop.interface",
        "gtk-theme",
        theme.gtk_theme(),
    ])
    .await;

    // `set` already told every subscriber the theme is `theme`. If the
    // write that *defines* the answer (`color-scheme` — the one key
    // `read_current` reads back) did not land, that was a lie with no
    // correction path: the handle would report a theme the session does not
    // have, permanently, since nothing else ever re-reads (#1192 review,
    // LOW-1). Re-read and correct, once, with one `warn!`.
    //
    // Guarded on the handle still holding the value we optimistically wrote:
    // a user who toggled again while this fan-out was in flight has a newer
    // intent, and a stale correction must not clobber it.
    if !color_scheme_ok && current_handle().get() == Some(theme) {
        let actual = read_current().await;
        current_handle().set(Some(actual));
        tracing::warn!(
            requested = ?theme,
            actual = ?actual,
            "theme: color-scheme write failed; reverted the current-theme handle to what gsettings reports",
        );
    }

    // Four synchronous read-modify-writes under `$HOME` (plus a
    // `create_dir_all` each). Blocking file I/O does not belong on an async
    // worker thread — the same defect, and the same fix, as `brightness`'s
    // 1 Hz sysfs walk in this issue (#1171): hand it to the blocking pool.
    // Awaited rather than detached so the `theme-changed` hook below still
    // fires strictly after the files it may want to read are written.
    let ini = tokio::task::spawn_blocking(move || {
        if let Err(e) = update_gtk_settings_ini("gtk-3.0", theme) {
            tracing::warn!(error = %e, "theme: gtk-3.0 settings.ini update failed");
        }
        if let Err(e) = update_gtk_settings_ini("gtk-4.0", theme) {
            tracing::warn!(error = %e, "theme: gtk-4.0 settings.ini update failed");
        }
        if let Err(e) = update_qtct_conf("qt5ct", theme) {
            tracing::warn!(error = %e, "theme: qt5ct.conf update failed");
        }
        if let Err(e) = update_qtct_conf("qt6ct", theme) {
            tracing::warn!(error = %e, "theme: qt6ct.conf update failed");
        }
    })
    .await;
    if let Err(e) = ini {
        tracing::warn!(error = %e, "theme: ini/conf update task panicked");
    }

    crate::hooks::run(
        "theme-changed",
        &[(
            "TROLLSHELL_THEME",
            match theme {
                Theme::Light => "light",
                Theme::Dark => "dark",
            },
        )],
    );
}

/// Run one `gsettings` mutation to completion on the tokio runtime,
/// returning whether it succeeded. `.status().await` (rather than the old
/// `std::process::Command::spawn()`, never `.wait()`ed) both keeps this off
/// the GTK thread and reaps the child immediately instead of leaking a
/// zombie until process exit.
///
/// The `bool` is what lets [`do_set`] correct an optimistic handle update
/// instead of leaving the process believing in a theme the session never
/// got (#1192 review, LOW-1).
async fn run_gsettings(args: &[&str]) -> bool {
    match tokio::process::Command::new("gsettings")
        .args(args)
        .status()
        .await
    {
        Ok(status) if status.success() => true,
        Ok(status) => {
            tracing::warn!(code = ?status.code(), args = ?args, "theme: gsettings exited non-zero");
            false
        }
        Err(e) => {
            tracing::warn!(error = %e, args = ?args, "theme: gsettings spawn failed");
            false
        }
    }
}

fn update_gtk_settings_ini(subdir: &str, theme: Theme) -> std::io::Result<()> {
    let path = config_subdir(subdir)?.join("settings.ini");
    let kvs: [(&str, &str); 2] = [
        (
            "gtk-application-prefer-dark-theme",
            if theme.is_dark() { "1" } else { "0" },
        ),
        ("gtk-theme-name", theme.gtk_theme()),
    ];
    update_ini_keys(&path, "Settings", &kvs)
}

fn update_qtct_conf(subdir: &str, theme: Theme) -> std::io::Result<()> {
    let path = config_subdir(subdir)?.join(format!("{subdir}.conf"));
    // Fusion is the always-available Qt built-in style. "Adwaita" /
    // "Adwaita-Dark" used to ship via adwaita-qt[5|6], dropped from Arch
    // repos in 2025. Fusion + qt[56]ct's bundled "darker" palette via
    // custom_palette gives equivalent visual coverage without extra
    // packages. Light mode unsets custom_palette so Fusion's built-in
    // light palette takes over.
    let dark_palette = format!("/usr/share/{subdir}/colors/darker.conf");
    let kvs: [(&str, &str); 3] = match theme {
        Theme::Dark => [
            ("style", "Fusion"),
            ("custom_palette", "true"),
            ("color_scheme_path", &dark_palette),
        ],
        Theme::Light => [
            ("style", "Fusion"),
            ("custom_palette", "false"),
            ("color_scheme_path", ""),
        ],
    };
    update_ini_keys(&path, "Appearance", &kvs)
}

fn config_subdir(name: &str) -> std::io::Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "HOME not set"))?;
    let dir = PathBuf::from(home).join(".config").join(name);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Tiny in-place ini editor: replaces matching `key=` lines inside
/// `[section]`, appends missing keys at the section's end (before any
/// trailing blank-line separator), and creates the section if it doesn't
/// exist. All other sections, comments, and unrelated keys are preserved
/// verbatim — this is critical for `qt[56]ct.conf` which the user may have
/// hand-edited or which may carry palette paths set by `qt[56]ct` itself.
fn update_ini_keys(path: &Path, section: &str, kvs: &[(&str, &str)]) -> std::io::Result<()> {
    let existing = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    let header = format!("[{section}]");
    let mut out = String::new();
    let mut in_target = false;
    let mut section_seen = false;
    let mut written: HashSet<String> = HashSet::new();
    // Trailing blank lines inside the target section are buffered so that
    // appended keys land immediately after the section's last real entry,
    // not after the blanks that separate the section from the next one.
    let mut blank_buffer = String::new();

    for line in existing.lines() {
        let t = line.trim();
        let is_section = t.starts_with('[') && t.ends_with(']');
        if is_section {
            if in_target {
                for &(k, v) in kvs {
                    if !written.contains(k) {
                        push_kv(&mut out, k, v);
                        written.insert(k.to_string());
                    }
                }
            }
            out.push_str(&blank_buffer);
            blank_buffer.clear();
            in_target = t == header;
            if in_target {
                section_seen = true;
            }
            out.push_str(line);
            out.push('\n');
            continue;
        }
        if t.is_empty() && in_target {
            blank_buffer.push_str(line);
            blank_buffer.push('\n');
            continue;
        }
        out.push_str(&blank_buffer);
        blank_buffer.clear();
        if in_target && let Some((lhs, _)) = t.split_once('=') {
            let key = lhs.trim();
            if let Some(&(_, v)) = kvs.iter().find(|(k, _)| *k == key) {
                push_kv(&mut out, key, v);
                written.insert(key.to_string());
                continue;
            }
        }
        out.push_str(line);
        out.push('\n');
    }
    if in_target {
        for &(k, v) in kvs {
            if !written.contains(k) {
                push_kv(&mut out, k, v);
                written.insert(k.to_string());
            }
        }
    }
    out.push_str(&blank_buffer);

    if !section_seen {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&header);
        out.push('\n');
        for &(k, v) in kvs {
            push_kv(&mut out, k, v);
        }
    }

    std::fs::File::create(path)?.write_all(out.as_bytes())?;
    Ok(())
}

fn push_kv(out: &mut String, k: &str, v: &str) {
    out.push_str(k);
    out.push('=');
    out.push_str(v);
    out.push('\n');
}

#[cfg(test)]
mod tests {
    use super::{Theme, read_current_from, spawn_seed, unseeded_handle, update_ini_keys};
    use futures_signals::signal::Signal;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn roundtrip(initial: &str, section: &str, kvs: &[(&str, &str)]) -> String {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hytte-theme-ini-test-{}-{n}.ini",
            std::process::id()
        ));
        std::fs::write(&path, initial).unwrap();
        update_ini_keys(&path, section, kvs).unwrap();
        let out = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        out
    }

    /// Write an executable stand-in for `gsettings` that prints `value` the
    /// way the real one does (a quoted string plus a newline) and exits 0.
    /// Returns its absolute path.
    fn fake_gsettings(value: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hytte-theme-fake-gsettings-{}-{n}",
            std::process::id()
        ));
        std::fs::write(&path, format!("#!/bin/sh\nprintf \"'{value}'\\n\"\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// #1192 review, HIGH-1 — the regression that shipped a "Dark mode: on"
    /// switch to a light session, permanently.
    ///
    /// Drives the real seed mechanism (`spawn_seed` + `read_current_from`)
    /// against a fake `gsettings` reporting `prefer-light`, through the real
    /// signal a panel binds, subscribing *before* the seed is spawned — i.e.
    /// exactly the order `panels/settings.rs` builds in. The assertion is on
    /// the whole observed sequence, not just the final value: the sequence
    /// must be `None` (unknown, switch insensitive) then `Some(Light)`, and
    /// must never contain `Some(Dark)`.
    ///
    /// **Falsification:** make `unseeded_handle` return
    /// `Mutable::new(Some(Theme::Dark))` (the pre-fix shape, where the
    /// fallback was a real `Theme` rather than `None`) and the first observed
    /// element becomes `Some(Dark)` — red on both assertions. That function
    /// is the production initial value, which is why this test calls it
    /// rather than writing its own `Mutable::new(None)`.
    #[tokio::test]
    async fn a_light_session_is_never_observed_as_dark() {
        let program = fake_gsettings("prefer-light");
        // Sanity: the fake really does read back as Light through the real
        // parser, so a failure below is about the *handle*, not the parse.
        assert_eq!(
            read_current_from(&program.display().to_string()).await,
            Theme::Light,
        );

        let handle = unseeded_handle();
        let mut signal = std::pin::pin!(handle.signal());
        let mut observed: Vec<Option<Theme>> = Vec::new();

        // Subscribe first — a `Mutable`'s signal always yields its current
        // value on the first poll, so this is the value a panel built before
        // the seed landed would render.
        observed.push(next_value(&mut signal).await);

        let seed_program = program.display().to_string();
        spawn_seed(&handle, move || async move {
            read_current_from(&seed_program).await
        });

        observed.push(next_value(&mut signal).await);
        let _ = std::fs::remove_file(&program);

        assert_eq!(
            observed,
            vec![None, Some(Theme::Light)],
            "a light session must be observed as unknown-then-Light",
        );
        assert!(
            !observed.contains(&Some(Theme::Dark)),
            "the Dark fallback must never be observed on a light session: {observed:?}",
        );
    }

    /// Next emission from `signal`, with a liveness budget so a broken seed
    /// fails the test instead of hanging the suite.
    async fn next_value<S>(signal: &mut std::pin::Pin<&mut S>) -> Option<Theme>
    where
        S: Signal<Item = Option<Theme>>,
    {
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            std::future::poll_fn(|cx| signal.as_mut().poll_change(cx)),
        )
        .await
        .expect("the seed read should land well inside the budget")
        .expect("the current-theme signal must not end")
    }

    #[test]
    fn theme_string_mappings() {
        assert_eq!(Theme::Light.color_scheme(), "prefer-light");
        assert_eq!(Theme::Dark.color_scheme(), "prefer-dark");
        assert_eq!(Theme::Light.gtk_theme(), "Adwaita");
        assert_eq!(Theme::Dark.gtk_theme(), "Adwaita-dark");
    }

    #[test]
    fn creates_file_with_section_when_absent() {
        let out = roundtrip("", "Settings", &[("gtk-theme-name", "Adwaita-dark")]);
        assert_eq!(out, "[Settings]\ngtk-theme-name=Adwaita-dark\n");
    }

    #[test]
    fn replaces_existing_key_in_section() {
        let initial = "[Settings]\ngtk-theme-name=Adwaita\nfoo=bar\n";
        let out = roundtrip(initial, "Settings", &[("gtk-theme-name", "Adwaita-dark")]);
        assert_eq!(out, "[Settings]\ngtk-theme-name=Adwaita-dark\nfoo=bar\n");
    }

    #[test]
    fn appends_missing_key_inside_section() {
        let initial = "[Settings]\nfoo=bar\n\n[Other]\nx=y\n";
        let out = roundtrip(initial, "Settings", &[("gtk-theme-name", "Adwaita-dark")]);
        assert_eq!(
            out,
            "[Settings]\nfoo=bar\ngtk-theme-name=Adwaita-dark\n\n[Other]\nx=y\n",
        );
    }

    #[test]
    fn appends_missing_section_at_end() {
        let initial = "[Other]\nx=y\n";
        let out = roundtrip(initial, "Settings", &[("gtk-theme-name", "Adwaita-dark")]);
        assert_eq!(
            out,
            "[Other]\nx=y\n\n[Settings]\ngtk-theme-name=Adwaita-dark\n",
        );
    }

    #[test]
    fn preserves_unrelated_sections_and_comments() {
        let initial = "# my conf\n[Other]\nx=y\n";
        let out = roundtrip(initial, "Settings", &[("gtk-theme-name", "Adwaita")]);
        assert_eq!(
            out,
            "# my conf\n[Other]\nx=y\n\n[Settings]\ngtk-theme-name=Adwaita\n",
        );
    }

    #[test]
    fn absent_file_treated_as_empty() {
        // A path that does not exist should be treated as empty (NotFound is
        // not an error — the file simply hasn't been created yet).
        let path = std::env::temp_dir().join(format!(
            "hytte-theme-ini-test-absent-{}.ini",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path); // ensure absent
        update_ini_keys(&path, "Settings", &[("gtk-theme-name", "Adwaita")]).unwrap();
        let out = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(out, "[Settings]\ngtk-theme-name=Adwaita\n");
    }

    #[test]
    fn unreadable_file_propagates_error() {
        // A non-NotFound IO error (e.g. IsADirectory) must propagate rather
        // than being silently swallowed as empty content.
        let dir = std::env::temp_dir().join(format!(
            "hytte-theme-ini-test-dir-{}.ini",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // Passing a directory path triggers ErrorKind::IsADirectory on Linux.
        let result = update_ini_keys(&dir, "Settings", &[("k", "v")]);
        let _ = std::fs::remove_dir(&dir);
        assert!(result.is_err(), "expected Err for unreadable path, got Ok");
    }
}
