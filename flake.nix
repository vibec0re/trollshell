{
  description = "trollshell — hytte-based Wayland desktop shell";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };

        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

        nativeBuildInputs = with pkgs; [
          rustToolchain
          pkg-config
          wrapGAppsHook4
          llvmPackages.libclang
        ];

        buildInputs = with pkgs; [
          glib
          gtk4
          libadwaita
          gtk4-layer-shell
          gsettings-desktop-schemas

          evolution-data-server
          libical
          gobject-introspection

          pam

          openssl
        ];
      in
      {
        devShells.default = pkgs.mkShell {
          inherit nativeBuildInputs buildInputs;

          packages = with pkgs; [
            rust-analyzer
          ];

          env = {
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
            BINDGEN_EXTRA_CLANG_ARGS = "-I${pkgs.pam}/include -I${pkgs.glibc.dev}/include";
          };

          shellHook = ''
            export RUST_BACKTRACE=1
          '';
        };

        formatter = pkgs.nixpkgs-fmt;
      });
}
