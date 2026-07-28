# Packages the `trollshell-control-center` companion binary (#399, split from
# #390): the external GTK4 + libadwaita settings & management app that talks to
# the running shell's `mov.vibec0re.trollshell.Control` session-bus endpoint. It
# ships as its own flake output alongside `.#trollshell`.
#
# THIS FILE RUNS NO CARGO AND NO CRANE (#572). `workspace` (nix/package.nix) is
# the single derivation that compiles the whole workspace; this slices its one
# binary out and wraps it. The previous shape was a second
# `craneLib.buildPackage` inheriting `workspace`'s packed `target` dir as
# `cargoArtifacts` and re-running `cargo build --workspace` in the hope that
# cargo would find everything fresh — #572 measured that hope and found the
# control center recompiled the same cascade the plugin packages did, so the
# #530 "reuse works for the second binary" premise never actually held.
#
# `workspace` installs raw, unwrapped ELFs (`dontWrapGApps = true` there) so the
# GTK-free plugin binaries don't drag a GTK closure. This is a normal windowed
# GTK app, so it gets wrapped here instead — with the *same* `buildInputs` the
# compile used (`workspace.passthru.devInputs`), so the GApplication
# environment the wrapper bakes in (`XDG_DATA_DIRS` / `GSETTINGS_SCHEMA_DIR` /
# `GI_TYPELIB_PATH`) is exactly what the in-crane wrapping produced before.
# Without it the GSettings-backed adwaita styling and symbolic icons would be
# missing. Unlike `trollshell` it reads no bundled assets (no
# `TROLLSHELL_DATA_DIR`), so there is nothing else to inject.
{
  lib,
  stdenv,
  makeDesktopItem,
  wrapGAppsHook4,
  # The single whole-workspace compile (nix/package.nix's `passthru.workspace`).
  workspace,
}:
let
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
    # (adwaita-icon-theme is in the shared buildInputs) so the launcher shows
    # something rather than a broken image.
    icon = "preferences-system";
    startupWMClass = "mov.vibec0re.trollshell.ControlCenter";
    categories = [
      "Settings"
      "GTK"
    ];
  };
in
stdenv.mkDerivation {
  pname = "trollshell-control-center";
  version = "0.1.0";

  dontUnpack = true;
  dontConfigure = true;
  dontBuild = true;

  nativeBuildInputs = [ wrapGAppsHook4 ];
  inherit (workspace.passthru.devInputs) buildInputs;

  installPhase = ''
    runHook preInstall
    install -Dm755 ${workspace}/bin/trollshell-control-center \
      "$out/bin/trollshell-control-center"
    mkdir -p "$out/share/applications"
    cp ${desktopItem}/share/applications/*.desktop "$out/share/applications/"
    runHook postInstall
  '';

  meta = {
    description = "trollshell control center — external GTK settings & management companion";
    homepage = "https://github.com/vibec0re/trollshell/";
    license = lib.licenses.mpl20;
    platforms = lib.platforms.linux;
    mainProgram = "trollshell-control-center";
  };
}
