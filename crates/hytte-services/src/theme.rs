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
//! zombie per gsettings call either. Failures on any one fan-out target are
//! logged and the others still run — best-effort, because partial coverage
//! is strictly better than aborting the whole switch on (e.g.) a missing
//! qt6ct dir.

use futures_signals::signal::Mutable;
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
static CURRENT: OnceLock<Mutable<Theme>> = OnceLock::new();

/// The shared current-theme handle. Seeded exactly once, the first time
/// anything asks for it, by spawning an async `gsettings get` on the tokio
/// runtime — never a blocking read on the caller's thread. Until that seed
/// read completes (a handful of milliseconds), [`current`] reports the
/// `Theme::Dark` fallback rather than block; `set()` keeps this handle in
/// sync going forward without ever re-querying gsettings.
fn current_handle() -> &'static Mutable<Theme> {
    CURRENT.get_or_init(|| {
        let mutable = Mutable::new(Theme::Dark);
        let writer = mutable.clone();
        runtime::handle().spawn(async move {
            writer.set(read_current().await);
        });
        mutable
    })
}

/// Current theme, as last known by this process. Never blocks and never
/// spawns a subprocess itself — see [`current_handle`] for how the value is
/// seeded and kept in sync. On any `gsettings` error, or a `default` value
/// (externally set, "follow system" — trollshell sessions don't have a
/// system to follow), the seed read resolves to `Theme::Dark`, matching
/// `adw::ColorScheme::PreferDark` defaults.
#[must_use]
pub fn current() -> Theme {
    current_handle().get()
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
    let output = tokio::process::Command::new("gsettings")
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
/// tokio runtime.
pub fn set(theme: Theme) {
    current_handle().set(theme);
    runtime::handle().spawn(async move {
        do_set(theme).await;
    });
}

/// The actual fan-out, run entirely on the tokio runtime (#1171) — every
/// subprocess spawn and every ini-file write happens here, never on the GTK
/// main thread.
async fn do_set(theme: Theme) {
    run_gsettings(&[
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

/// Run one `gsettings` mutation to completion on the tokio runtime.
/// `.status().await` (rather than the old `std::process::Command::spawn()`,
/// never `.wait()`ed) both keeps this off the GTK thread and reaps the child
/// immediately instead of leaking a zombie until process exit.
async fn run_gsettings(args: &[&str]) {
    match tokio::process::Command::new("gsettings")
        .args(args)
        .status()
        .await
    {
        Ok(status) if status.success() => {}
        Ok(status) => {
            tracing::warn!(code = ?status.code(), args = ?args, "theme: gsettings exited non-zero");
        }
        Err(e) => tracing::warn!(error = %e, args = ?args, "theme: gsettings spawn failed"),
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
    use super::{Theme, update_ini_keys};
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
