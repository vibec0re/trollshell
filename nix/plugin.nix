# Packages a single workspace binary — a bundled widget plugin, or a standalone
# CLI tool like `hytte-infobroker` (#562) — as its own flake output (#558) so
# `programs.trollshell.plugins.<id>.package` (for plugins) or a plain
# `home.packages` entry (for tools) finally has something in THIS flake to
# point at. Before this the workspace build compiled all 12 `hytte-plugin-*`
# binaries, but every package derivation pruned `$out/bin` down to its own
# binary (#530), so each bundled plugin was buildable yet never shippable — the
# options doc couldn't honestly document how to enable one (#457, #558). One
# parameterized derivation, called once per binary from flake.nix.
#
# Like `nix/control-center.nix` (#530), this reuses the shell package's shared
# `workspaceArtifacts` + `commonArgs` rather than run a second (~30-min) deps
# build or recompile the workspace. `workspaceArtifacts` is the intermediate
# `cargoBuild` that already compiled the ENTIRE workspace once (nix/package.nix),
# so this derivation finds every plugin binary already built in the warm target
# dir and does little more than link + install its own — seconds apiece.
#
# The build environment is inherited from `commonArgs` UNCHANGED — including its
# `wrapGAppsHook4` in `nativeBuildInputs`. That is load-bearing for the warm
# reuse: cargo/build-script fingerprints key on the build-phase environment, and
# `wrapGAppsHook4`'s setup hook exports env (XDG_DATA_DIRS/GSETTINGS_SCHEMA_DIR/
# GI_TYPELIB_PATH) that the gtk-rs `*-sys` build scripts observe. Dropping the
# hook here (an earlier attempt did) shifts that env away from what
# `workspaceArtifacts` was built with, so cargo invalidates and recompiles the
# `*-sys` deps + the whole workspace per plugin — defeating the entire #530
# reuse. So we keep the hook to match the environment, and instead set
# `dontWrapGApps` to skip the actual wrapping (below).
#
# Differences from control-center.nix:
#
#   - `dontWrapGApps = true`. Plugins are GTK-free by design — a plugin ships a
#     declarative widget tree over `hytte-plugin-proto` and the *host* renders
#     it (crates/hytte-plugin/README), so the binary links no GTK and needs no
#     GApplication schema/icon environment. `dontWrapGApps` disables only
#     wrapGAppsHook4's fixup-phase wrapping (its setup hook still runs, keeping
#     the build env — and thus the fingerprints — identical to
#     `workspaceArtifacts`), so the installed binary stays a plain unwrapped ELF
#     and doesn't drag the Adwaita/GSettings closure at runtime.
#   - NO .desktop launcher. A plugin is launched by the shell as a transient
#     `trollshell-plugin-<id>` user unit (trollshell/src/plugin_launcher.rs),
#     never from an application menu.
#
# `meta.mainProgram` is set to the binary name so `lib.getExe plugin.package`
# — how nix/{hm,nixos}-module.nix derive each plugin unit's ExecStart — resolves.
{
  lib,
  craneLib,
  trollshell,
  # The plugin crate = binary = flake-output name, e.g. "hytte-plugin-pet".
  # Bundled plugins name their binary after the crate
  # (crates/hytte-plugin-<id>/Cargo.toml's [[bin]]).
  name,
}:
let
  inherit (trollshell.passthru) commonArgs workspaceArtifacts;
in
craneLib.buildPackage (
  commonArgs
  // {
    cargoArtifacts = workspaceArtifacts;
    pname = name;

    # Build against the already-compiled whole-workspace target so nothing is
    # recompiled here (#530). MUST build `--workspace` (not `-p …`) to
    # feature-match that target — a `-p` scope would fingerprint-mismatch and
    # recompile ~all of it (see the feature-unification note in package.nix).
    cargoExtraArgs = "--workspace";

    # No test run here: the hermetic internals suite runs in the trollshell
    # package's check phase (the shared workspaceArtifacts stage is build-only to
    # stay feature-pristine — see package.nix). Matches control-center.nix.
    doCheck = false;

    # Plugins are GTK-free (see the header): keep wrapGAppsHook4 present (its
    # setup hook keeps the build env matching workspaceArtifacts, so the warm
    # target is reused) but skip the fixup-phase wrapping, leaving an unwrapped
    # binary with no GTK/Adwaita runtime closure.
    dontWrapGApps = true;

    # A `--workspace` build installs every workspace binary from the build log;
    # keep ONLY this plugin's binary. (The infobroker crate also builds a
    # `hytte-infobroker` CLI — this prunes it away; the plugin package ships just
    # the plugin binary, which is what `plugins.<id>.package` points at.)
    postInstall = ''
      find "$out/bin" -mindepth 1 -maxdepth 1 ! -name ${name} -exec rm -rf {} +
    '';

    meta = {
      description = "trollshell bundled widget plugin — ${name}";
      homepage = "https://github.com/vibec0re/trollshell/";
      license = lib.licenses.mpl20;
      platforms = lib.platforms.linux;
      mainProgram = name;
    };
  }
)
