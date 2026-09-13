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
# `workspace` (nix/package.nix) is the single derivation that compiles the
# whole workspace's default feature set — the shell, the control center,
# every plugin. Until #1257 the two probe examples rode along inside IT (a
# `postInstall` after the workspace build), which meant every one of those
# consumers recompiled hytte-ecal's and hytte-services' dev-dependency
# closures too, on a cache (`cargoArtifactsBinOnly`) that holds none. Since
# #1257 the examples are `probes` (nix/package.nix's `passthru.probes`), a
# SEPARATE crane compile in the checks universe, on `cargoArtifacts` — the
# same dev-deps cache `checks.{clippy,system-tests,workspace-tests}` already
# share. That does NOT make it free: measured (2026-09-13), `probes` still
# runs ~101 `Compiling` lines in ~3m14s — roughly what the pre-#1257
# `postInstall` cost (~106/~3m04s), because it's a `-p`-scoped build against
# a `--workspace`-scoped cache (see `nix/package.nix`'s `probes` comment).
# What changed is WHO pays it: no package build (this one included) reaches
# `probes` any more, so the cost moves out of every consumer's `nix build`
# and into one checks-universe derivation, once per Cargo.lock/source
# change, instead of once per consumer. `$out/bin/wifi_probe` comes from
# `${probes}` now, not `${workspace}` — `workspace` is still taken here, but
# only for `passthru.devInputs`, the GApps wrap's `buildInputs`.
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
  # The single whole-workspace compile (nix/package.nix's `passthru.workspace`)
  # — taken here ONLY for `passthru.devInputs.buildInputs`, the GApps wrap
  # env; the binary itself comes from `probes` (#1257).
  workspace,
  # The checks-universe probe-examples compile (nix/package.nix's
  # `passthru.probes`) — the actual source of `$out/bin/wifi_probe` since
  # #1257.
  probes,
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
    install -Dm755 ${probes}/bin/wifi_probe "$out/bin/wifi_probe"
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
