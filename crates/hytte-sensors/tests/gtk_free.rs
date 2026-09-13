//! Pins hytte-sensors' GTK-free-by-construction guarantee (#1249): this leaf
//! exists so a future plugin (#1250, `hytte-plugin-stats`) can reuse the
//! same procfs/sysfs samplers `hytte-services::sensors` wraps, without
//! pulling in GTK, D-Bus, or the reactive registry the way linking
//! `hytte-services` itself would. #1249's own pin names `cargo tree
//! -p hytte-sensors -e normal` as the check; this test reads the crate's own
//! `Cargo.toml` instead, which pins the identical fact hermetically — no
//! `cargo` subprocess, no registry or lockfile access — and reds the moment
//! a `[dependencies]` line names something this leaf must never carry.
//!
//! Falsification: add `glib = { workspace = true }` (or any of the other
//! four names below) under `[dependencies]` in `crates/hytte-sensors/
//! Cargo.toml` and this test fails; removing it again turns it back green.
//! Verified by hand for this PR (#1249) rather than left as a standing
//! self-mutating test.

const FORBIDDEN: &[&str] = &["gtk4", "glib", "gio", "zbus", "hytte-"];

#[test]
fn cargo_toml_names_no_gtk_or_hytte_dependency() {
    let manifest = include_str!("../Cargo.toml");
    let deps_section = manifest
        .split_once("[dependencies]")
        .map(|(_, rest)| rest)
        .expect("Cargo.toml must have a [dependencies] table");
    // Stop at the next top-level table header so `[dev-dependencies]` (which
    // may reasonably need something a shipped build never does) isn't
    // scanned by this pin — only what a *consumer's* build actually links.
    // Strip `#`-comments per line first: this file's own doc comments above
    // the `nix` entry say "hytte-services" in prose, which would otherwise
    // false-positive the `hytte-` check below.
    let deps_only: String = deps_section
        .lines()
        .take_while(|line| !line.trim_start().starts_with('['))
        .map(|line| line.split('#').next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");

    for forbidden in FORBIDDEN {
        assert!(
            !deps_only.contains(forbidden),
            "hytte-sensors/Cargo.toml's [dependencies] names `{forbidden}` — \
             this leaf crate must stay GTK-free and hytte-*-free (#1249)"
        );
    }
}
