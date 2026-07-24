# Packages the `trollshell-control-center` companion binary (#399, split from
# #390): the external GTK4 + libadwaita settings & management app that talks to
# the running shell's `mov.vibec0re.trollshell.Control` session-bus endpoint. It
# ships as its own flake output alongside `.#trollshell`.
#
# Rather than run a second (~30-min) deps build — or recompile every hytte-*
# workspace crate a second time — this reuses the shell package's shared
# `workspaceArtifacts` + `commonArgs` (#530). `workspaceArtifacts` is the
# intermediate `cargoBuild` that already compiled the ENTIRE workspace once (see
# nix/package.nix), so this derivation finds every crate it links already built
# in the warm target dir and does little more than link + install its own
# binary. (The earlier #411 sharing reused `trollshell.passthru.cargoArtifacts`,
# but that's crane's `buildDepsOnly` output — external crates only, workspace
# members stubbed — so it still recompiled all of hytte-* here; #530 fixes that.
# The `clippy` / `system-tests` flake checks still reuse the deps-only
# `cargoArtifacts` because they compile a different feature set.)
#
# `commonArgs.nativeBuildInputs` already carries `wrapGAppsHook4`, so the crane
# build wraps the binary with the GApplication schema/icon/resource environment
# (`XDG_DATA_DIRS`/`GSETTINGS_SCHEMA_DIR`/`GI_TYPELIB_PATH`) exactly like the
# shell binary gets — this is a normal windowed GTK app, so without it the
# GSettings-backed adwaita styling and symbolic icons would be missing. Unlike
# `trollshell` it reads no bundled assets (no `TROLLSHELL_DATA_DIR`), so it needs
# no extra makeWrapper asset layer — the crane output is the final package.
{
  lib,
  craneLib,
  makeDesktopItem,
  trollshell,
}:
let
  inherit (trollshell.passthru) commonArgs workspaceArtifacts;

  # A .desktop launcher named after the app-id so the app is startable by name
  # (and window-matched via StartupWMClass). Exec is the bare binary name — it
  # resolves on PATH once the package is on the system/user profile.
  desktopItem = makeDesktopItem {
    name = "mov.vibec0re.trollshell.ControlCenter";
    desktopName = "trollshell Control Center";
    genericName = "Desktop Shell Settings";
    comment = "Settings & management companion for the trollshell desktop shell";
    exec = "trollshell-control-center";
    # No bundled app icon yet; use a stock settings glyph from the Adwaita theme
    # (adwaita-icon-theme is in commonArgs.buildInputs) so the launcher shows
    # something rather than a broken image.
    icon = "preferences-system";
    startupWMClass = "mov.vibec0re.trollshell.ControlCenter";
    categories = [
      "Settings"
      "GTK"
    ];
  };
in
craneLib.buildPackage (
  commonArgs
  // {
    cargoArtifacts = workspaceArtifacts;
    pname = "trollshell-control-center";

    # Build against the already-compiled whole-workspace target so nothing is
    # recompiled here (#530). MUST build `--workspace` (not `-p …`) to
    # feature-match that target — a `-p` scope would fingerprint-mismatch and
    # recompile ~all of it (see the feature-unification note in package.nix).
    cargoExtraArgs = "--workspace";

    # No test run here: the hermetic internals suite runs in the trollshell
    # package's check phase (the shared workspaceArtifacts stage is build-only to
    # stay feature-pristine — see the notes in package.nix). This matches the
    # pre-#530 behaviour, where the control-center package never ran tests.
    doCheck = false;

    # A `--workspace` build installs every workspace binary from the build log;
    # keep ONLY the control-center binary, then drop the .desktop launcher into
    # place. Both run before wrapGAppsHook4's fixup, so only the surviving
    # binary gets wrapped.
    postInstall = ''
      find "$out/bin" -mindepth 1 -maxdepth 1 ! -name trollshell-control-center -exec rm -rf {} +
      mkdir -p $out/share/applications
      cp ${desktopItem}/share/applications/*.desktop $out/share/applications/
    '';

    meta = {
      description = "trollshell control center — external GTK settings & management companion";
      homepage = "https://github.com/vibec0re/trollshell/";
      license = lib.licenses.mpl20;
      platforms = lib.platforms.linux;
      mainProgram = "trollshell-control-center";
    };
  }
)
