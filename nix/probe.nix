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
# `workspace` (nix/package.nix) is the single derivation that compiles the whole
# workspace, and since #588 its `$out/bin` carries the two probe examples too:
# cargo's default `cargo test` target selection builds every example "to ensure
# they compile", so the workspace build's `doCheck` phase already produces them
# and a postInstall hook copies them out of the target dir. Packaging one here
# is a `cp` + a wrap.
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
  # The single whole-workspace compile (nix/package.nix's `passthru.workspace`).
  workspace,
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
    install -Dm755 ${workspace}/bin/probe "$out/bin/probe"
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
