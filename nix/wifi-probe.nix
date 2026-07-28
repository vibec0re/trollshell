# Packages the `hytte-services` `wifi_probe` example binary for the
# NetworkManager Wi-Fi nixosTest (checks.wifi-nm-nixos-test), which boots a VM
# with NetworkManager and a pair of simulated `mac80211_hwsim` radios and drives
# `wifi_nm` against the live daemon.
#
# THIS FILE RUNS NO CARGO AND NO CRANE (#588, finishing #572's step 4 across the
# whole tree) — see nix/probe.nix's header for the full rationale; this is the
# same slice, for the other example binary. Until #588 it carried its own
# `craneLib.buildDepsOnly` + `buildPackage` pair and its own `src` filter, so a
# cold `nix flake check` paid a third full dependency compile just for this one
# binary.
#
# `workspace` (nix/package.nix) is the single derivation that compiles the whole
# workspace, and its `doCheck` test phase builds the example targets too (cargo
# builds every example during `cargo test` "to ensure they compile"), so
# `$out/bin/wifi_probe` already exists there.
#
# The wrap is preserved from the pre-#588 shape for the same reason as
# nix/probe.nix: the old derivation had `wrapGAppsHook4` in `nativeBuildInputs`
# without `dontWrapGApps`, so this binary was GApps-wrapped. `workspace`
# installs raw ELFs (`dontWrapGApps = true` there), so the wrapping happens here
# instead, over the *same* `buildInputs` the compile used
# (`workspace.passthru.devInputs`). This probe is D-Bus-only and would very
# likely work unwrapped, but #588 is a packaging consolidation, not a behaviour
# change — keeping the wrapper means the binary the VM runs is byte-for-byte the
# same shape as before.
{
  lib,
  stdenv,
  wrapGAppsHook4,
  # The single whole-workspace compile (nix/package.nix's `passthru.workspace`).
  workspace,
}:
stdenv.mkDerivation {
  pname = "hytte-services-wifi-probe";
  version = "0.1.0";

  dontUnpack = true;
  dontConfigure = true;
  dontBuild = true;

  nativeBuildInputs = [ wrapGAppsHook4 ];
  inherit (workspace.passthru.devInputs) buildInputs;

  installPhase = ''
    runHook preInstall
    install -Dm755 ${workspace}/bin/wifi_probe "$out/bin/wifi_probe"
    runHook postInstall
  '';

  meta = {
    description = "hytte-services NetworkManager Wi-Fi probe example binary (checks.wifi-nm-nixos-test)";
    homepage = "https://github.com/vibec0re/trollshell/";
    license = lib.licenses.mpl20;
    platforms = lib.platforms.linux;
    mainProgram = "wifi_probe";
  };
}
