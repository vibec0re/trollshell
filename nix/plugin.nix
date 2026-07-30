# Packages a single workspace binary — a bundled widget plugin, a standalone
# CLI tool like `hytte-infobroker` (#562), or a standalone GTK-free daemon like
# `hytte-claude-bridge` (#584) — as its own flake output (#558) so
# `programs.trollshell.plugins.<id>.package` (for plugins) or a plain
# `home.packages` entry (for tools) finally has something in THIS flake to
# point at. Before this the workspace build compiled all 12 `hytte-plugin-*`
# binaries, but every package derivation pruned `$out/bin` down to its own
# binary (#530), so each bundled plugin was buildable yet never shippable — the
# options doc couldn't honestly document how to enable one (#457, #558). One
# parameterized derivation, called once per binary from flake.nix.
#
# THIS FILE RUNS NO CARGO AND NO CRANE (#572). `workspace` (nix/package.nix) is
# the single derivation that compiles the whole workspace; every binary the
# flake ships already exists in its `$out/bin`, so packaging one is a `cp`.
#
# That is the whole point of #572. The previous shape here was a second
# `craneLib.buildPackage` that inherited `workspace`'s packed `target` dir as
# its `cargoArtifacts` and re-ran `cargo build --workspace`, expecting cargo to
# find everything fresh and just re-install. Measured, it did not: each of the
# 13 packages recompiled the workspace (~40 min apiece locally; ~40 min of extra
# parallel work on every CI run once #561 wired them all into `checks`).
# Inheriting a warm target dir across derivations is a cache *hope* that any
# fingerprint drift silently voids; slicing one already-built output is a
# guarantee, and it cannot regress.
#
# The old header here blamed the (now-deleted) `dontWrapGApps = true` for
# needing `wrapGAppsHook4` kept in `nativeBuildInputs` to "match the build
# environment". That rationale was a misdiagnosis: `dontWrapGApps` only gates
# wrapGAppsHook4's fixup-phase wrapper array and provably cannot affect the
# build environment cargo fingerprints, and a drv diff showed the environments
# were already identical. The recompiles had another cause; the fix is to not
# have a second compile at all.
#
# Plugins are GTK-free by design — a plugin ships a declarative widget tree over
# `hytte-plugin-proto` and the *host* renders it (crates/hytte-plugin/README) —
# so there is nothing to wrap: `workspace` installs raw ELFs
# (`dontWrapGApps = true` there) and this copies one out as-is, with no GTK /
# Adwaita / GSettings runtime closure. A plugin is launched by the shell as a
# transient `trollshell-plugin-<id>` user unit
# (trollshell/src/plugin_launcher.rs), never from an application menu, so there
# is no .desktop launcher either.
#
# `meta.mainProgram` is set to the binary name so `lib.getExe plugin.package`
# — how nix/{hm,nixos}-module.nix derive each plugin unit's ExecStart — resolves.
{
  lib,
  runCommand,
  # The single whole-workspace compile (nix/package.nix's `passthru.workspace`).
  workspace,
  # The plugin crate = binary = flake-output name, e.g. "hytte-plugin-pet".
  # Bundled plugins name their binary after the crate
  # (crates/hytte-plugin-<id>/Cargo.toml's [[bin]]).
  name,
  # `meta.description`. The default fits the bundled widget plugins; standalone
  # tools (hytte-infobroker) pass their own so the description doesn't
  # over-claim plugin-hood.
  description ? "trollshell bundled widget plugin — ${name}",
}:
runCommand "${name}-0.1.0"
  {
    meta = {
      inherit description;
      homepage = "https://github.com/vibec0re/trollshell/";
      license = lib.licenses.mpl20;
      platforms = lib.platforms.linux;
      mainProgram = name;
    };
  }
  ''
    install -Dm755 ${workspace}/bin/${name} "$out/bin/${name}"
  ''
