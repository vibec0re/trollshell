# Packages the `trollshell-control-center` companion binary (#399, split from
# #390): the external GTK4 + libadwaita settings & management app that talks to
# the running shell's `mov.vibec0re.trollshell.Control` session-bus endpoint. It
# ships as its own flake output alongside `.#trollshell`.
#
# Rather than run a second (~30-min) deps build, this reuses the shell package's
# shared `cargoArtifacts` + `commonArgs` (same workspace `Cargo.lock`, so the
# external-dependency closure is identical) and only recompiles the handful of
# workspace members the control-center actually links — exactly how the `clippy`
# and `system-tests` flake checks reuse `trollshell.passthru.cargoArtifacts`.
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
  inherit (trollshell.passthru) commonArgs cargoArtifacts;

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
    inherit cargoArtifacts;
    pname = "trollshell-control-center";

    # Build ONLY the control-center binary; the reused cargoArtifacts already
    # holds the whole shell's dependency closure (a superset of this crate's).
    cargoExtraArgs = "-p trollshell-control-center";

    # The hermetic internals suite already runs in the shell package build
    # (commonArgs.doCheck = true, --workspace); no need to re-run it here.
    doCheck = false;

    # Drop the .desktop launcher into place; wrapGAppsHook4 (in commonArgs's
    # nativeBuildInputs) then wraps the binary in the fixup phase.
    postInstall = ''
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
