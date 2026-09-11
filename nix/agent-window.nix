# Packages the `trollshell-agent-window` companion binary (#950, phase P2 of
# #947): the per-agent window that puts hyperhive's own agent page in a
# WebKitGTK view inside trollshell's chrome. It ships as its own flake output
# alongside `.#trollshell`, and the agents plugin launches it out of process
# (a detached `RunCommand`, #953) by name off `PATH`.
#
# THIS FILE RUNS NO CARGO AND NO CRANE (#572/#587). `workspace`
# (nix/package.nix) is the single derivation that compiles the whole workspace;
# this slices its one binary out and wraps it. Adding a `craneLib.buildPackage`
# here would recompile the workspace — measured, that is what every one of the
# 13 pre-#587 per-binary derivations did.
#
# It is `nix/control-center.nix`'s shape, not `nix/plugin.nix`'s, and the
# difference is the wrap: `workspace` installs raw, unwrapped ELFs
# (`dontWrapGApps = true` there) because the plugin binaries are GTK-free. This
# is a normal windowed GTK4 + libadwaita app — and a WebKitGTK one, which needs
# the GApplication environment even more than the control center does: WebKit
# loads its own GIO modules and GSettings schemas at runtime. So it is wrapped
# here, with the *same* `buildInputs` the compile used
# (`workspace.passthru.devInputs`), so the env the wrapper bakes in
# (`XDG_DATA_DIRS` / `GSETTINGS_SCHEMA_DIR` / `GI_TYPELIB_PATH`) is exactly what
# an in-crane wrapping would have produced.
#
# **No .desktop item**, deliberately, where the control center has one: this
# window is meaningless without `--agent <name>`, so a menu entry would launch
# something that prints a usage line and exits. It is reached from the agents
# card, or typed. Its window is matchable by app-id all the same — one id per
# agent under `mov.vibec0re.trollshell.AgentWindow.`, so a niri rule wanting all
# of them matches the prefix (see docs/live-verify.md).
{
  lib,
  stdenv,
  wrapGAppsHook4,
  # The single whole-workspace compile (nix/package.nix's `passthru.workspace`).
  workspace,
  # Source revision string (#601), computed once in flake.nix from
  # `self.shortRev` / `self.dirtyShortRev`. Defaults so an out-of-flake
  # `callPackage ./nix/agent-window.nix` still evaluates.
  revision ? "unknown",
}:
stdenv.mkDerivation {
  pname = "trollshell-agent-window";
  version = "0.1.0";

  dontUnpack = true;
  dontConfigure = true;
  dontBuild = true;

  nativeBuildInputs = [ wrapGAppsHook4 ];
  inherit (workspace.passthru.devInputs) buildInputs;

  installPhase = ''
    runHook preInstall
    install -Dm755 ${workspace}/bin/trollshell-agent-window \
      "$out/bin/trollshell-agent-window"
    runHook postInstall
  '';

  # The build revision (#601), on the wrapper wrapGAppsHook4 already creates —
  # never as a compile-time env, which would rehash the one expensive
  # `workspace` compile on every commit (see nix/package.nix).
  preFixup = ''
    gappsWrapperArgs+=(
      --set TROLLSHELL_REV "${revision}"
    )
  '';

  meta = {
    description = "trollshell agent window — hyperhive's agent page in a WebKitGTK view, in our chrome";
    homepage = "https://github.com/vibec0re/trollshell/";
    license = lib.licenses.mpl20;
    platforms = lib.platforms.linux;
    mainProgram = "trollshell-agent-window";
  };
}
