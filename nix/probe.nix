# Packages the `hytte-ecal` `probe` example binary for the EDS nixosTest
# (checks.eds-nixos-test), which boots a real evolution-data-server in a VM and
# drives libecal against it end-to-end.
#
# THIS FILE RUNS NO CARGO AND NO CRANE (#588, finishing #572's step 4 across the
# whole tree). Until #588 it carried its own `craneLib.buildDepsOnly` +
# `buildPackage` pair — *and* its own `src` filter, so it didn't even share the
# `trollshell-source` derivation — which meant every cold `nix flake check` paid
# a second full dependency compile (~470 crates) purely to produce this one
# example binary. `nix/wifi-probe.nix` paid a third.
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
# share. That did NOT make it free at first: measured (2026-09-13), `probes`
# ran ~101 `Compiling` lines in ~3m14s — roughly what the pre-#1257
# `postInstall` cost (~106/~3m04s), because it was a `-p`-scoped build
# against a `--workspace`-scoped cache. #1276 fixed the scope mismatch
# itself (`--workspace --example probe --example wifi_probe`, matching the
# cache's own scope): measured (2026-09-14), `Compiling` dropped to 11 — see
# `nix/package.nix`'s `probes` comment for the full breakdown.
# What changed is WHO pays it: no package build (this one included) reaches
# `probes` any more, so the cost moves out of every consumer's `nix build`
# and into one checks-universe derivation, once per Cargo.lock/source
# change, instead of once per consumer. Packaging one here is still just a
# `cp` + a wrap, now out of `${probes}/bin/probe` instead of
# `${workspace}/bin/probe` — `workspace` is still taken here, but only for
# `passthru.devInputs`, the GApps wrap's `buildInputs`.
#
# The wrap is load-bearing and preserved verbatim from the pre-#588 shape. The
# old derivation had `wrapGAppsHook4` in `nativeBuildInputs` and did *not* set
# `dontWrapGApps`, so `$out/bin/probe` came out GApps-wrapped —
# `GIO_EXTRA_MODULES` (the dconf GSettings backend EDS's source registry wants),
# `GI_TYPELIB_PATH`, `XDG_DATA_DIRS`, `GDK_PIXBUF_MODULE_FILE`. `workspace`
# installs raw, unwrapped ELFs (`dontWrapGApps = true` there) so the GTK-free
# plugin binaries don't drag a GTK closure, so the wrapping moves here instead —
# using the *same* `buildInputs` the compile used
# (`workspace.passthru.devInputs`), so the injected environment is unchanged.
# Same arrangement, same reason, as nix/control-center.nix.
{
  lib,
  stdenv,
  wrapGAppsHook4,
  # The single whole-workspace compile (nix/package.nix's `passthru.workspace`)
  # — taken here ONLY for `passthru.devInputs.buildInputs`, the GApps wrap
  # env; the binary itself comes from `probes` (#1257).
  workspace,
  # The checks-universe probe-examples compile (nix/package.nix's
  # `passthru.probes`) — the actual source of `$out/bin/probe` since #1257.
  probes,
}:
stdenv.mkDerivation {
  pname = "hytte-ecal-probe";
  version = "0.1.0";

  dontUnpack = true;
  dontConfigure = true;
  dontBuild = true;

  nativeBuildInputs = [ wrapGAppsHook4 ];
  inherit (workspace.passthru.devInputs) buildInputs;

  installPhase = ''
    runHook preInstall
    install -Dm755 ${probes}/bin/probe "$out/bin/probe"
    runHook postInstall
  '';

  meta = {
    description = "hytte-ecal EDS probe example binary (checks.eds-nixos-test)";
    homepage = "https://github.com/vibec0re/trollshell/";
    license = lib.licenses.mpl20;
    platforms = lib.platforms.linux;
    mainProgram = "probe";
  };
}
