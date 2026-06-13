{
  lib,
  makeRustPlatform,
  rust-bin,
  pkg-config,
  wrapGAppsHook4,
  llvmPackages,
  glibc,
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
  rustToolchain = rust-bin.fromRustupToolchainFile ../rust-toolchain.toml;

  rustPlatform = makeRustPlatform {
    cargo = rustToolchain;
    rustc = rustToolchain;
  };
in
rustPlatform.buildRustPackage {
  pname = "trollshell";
  version = "0.1.0";
  src = ../.;

  cargoLock = {
    lockFile = ../Cargo.lock;
  };

  # Workspace has multiple binaries; we only need the trollshell one.
  cargoBuildFlags = [
    "-p"
    "trollshell"
  ];
  # Tests touch live system daemons (dbus, etc.); skip in nix sandbox.
  doCheck = false;

  nativeBuildInputs = [
    rustToolchain
    pkg-config
    wrapGAppsHook4
    llvmPackages.libclang
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

    # libpipewire-0.3 + libspa-0.2 — pipewire-rs (libpipewire-sys /
    # libspa-sys) discovers headers + .so via pkg-config (.pc files
    # ship in the dev output and pkg-config is already in
    # nativeBuildInputs).
    pipewire
  ];

  env = {
    LIBCLANG_PATH = "${llvmPackages.libclang.lib}/lib";
    BINDGEN_EXTRA_CLANG_ARGS = "-I${glibc.dev}/include";
    # Baked into the binary at compile time; trollshell::assets reads
    # this with option_env! and falls back to CARGO_MANIFEST_DIR when
    # unset (the dev `cargo run` case).
    TROLLSHELL_DATA_DIR = "${placeholder "out"}/share/trollshell";
  };

  postInstall = ''
    mkdir -p $out/share/trollshell
    cp -r trollshell/icons $out/share/trollshell/
    cp trollshell/style.css $out/share/trollshell/
  '';

  meta = {
    description = "hytte-based Wayland desktop shell";
    homepage = "https://git.hannig.cc/choom/trollshell";
    license = lib.licenses.mpl20;
    platforms = lib.platforms.linux;
    mainProgram = "trollshell";
  };
}
