# Builds the `hytte-services` `wifi_probe` example binary for the
# NetworkManager Wi-Fi nixosTest (checks.wifi-nm-nixos-test). Mirrors
# probe.nix's crane setup — the deps build still compiles the whole workspace
# lock (including hytte-ecal which links libecal), so it needs the same full
# buildInputs + bindgen/pipewire handling — but targets the wifi_probe example
# instead of the EDS probe binary.
{
  lib,
  craneLib,
  rustPlatform,
  pkg-config,
  wrapGAppsHook4,
  glib,
  gtk4,
  libadwaita,
  gtk4-layer-shell,
  gsettings-desktop-schemas,
  adwaita-icon-theme,
  hicolor-icon-theme,
  evolution-data-server,
  libical,
  gobject-introspection,
  openssl,
  pipewire,
}:
let
  src = lib.cleanSourceWith {
    src = ../.;
    name = "trollshell-source";
    filter =
      path: type:
      (craneLib.filterCargoSources path type)
      || (lib.hasSuffix ".css" path)
      || (lib.hasInfix "/assets/trollshell/icons/" path)
      || (lib.hasInfix "/tests/fixtures/" path);
  };

  nativeBuildInputs = [
    pkg-config
    wrapGAppsHook4
    rustPlatform.bindgenHook
  ];

  buildInputs = [
    glib
    gtk4
    libadwaita
    gtk4-layer-shell
    gsettings-desktop-schemas
    adwaita-icon-theme
    hicolor-icon-theme
    evolution-data-server
    libical
    gobject-introspection
    openssl
    pipewire
  ];

  commonArgs = {
    pname = "hytte-services-wifi-probe";
    version = "0.1.0";
    inherit src nativeBuildInputs buildInputs;
    cargoExtraArgs = "-p hytte-services --example wifi_probe";
    doCheck = false;
    # Same libspa-bindgen-needs-a-writable-vendor-dir workaround as package.nix.
    preBuild = ''
      writableVendor="$NIX_BUILD_TOP/writable-vendor"
      cp -rL --no-preserve=mode,ownership "$cargoVendorDir" "$writableVendor"
      chmod -R u+w "$writableVendor"
      substituteInPlace "$CARGO_HOME/config.toml" \
        --replace-fail "$cargoVendorDir" "$writableVendor"
    '';
  };

  # The deps build runs against crane's stub sources, which have no `examples/`
  # dir — so scope it to the whole workspace's deps (lib/bin targets) rather
  # than the `wifi_probe` example target, which only exists in the real source
  # used by buildPackage below.
  cargoArtifacts = craneLib.buildDepsOnly (commonArgs // { cargoExtraArgs = "--workspace"; });
in
craneLib.buildPackage (
  commonArgs
  // {
    inherit cargoArtifacts;
    # crane installs declared bins from the build log; an `--example` binary
    # isn't always picked up, so install it explicitly from the target dir.
    postInstall = ''
      if [ ! -e "$out/bin/wifi_probe" ]; then
        bin="$(find target -type f -name wifi_probe -path '*examples*' | head -1)"
        install -Dm755 "$bin" "$out/bin/wifi_probe"
      fi
    '';
  }
)
