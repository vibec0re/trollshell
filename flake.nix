{
  description = "trollshell — hytte-based Wayland desktop shell";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    treefmt-nix = {
      url = "github:numtide/treefmt-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
      treefmt-nix,
      ...
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems =
        fn:
        nixpkgs.lib.genAttrs systems (
          system:
          fn rec {
            pkgs = import nixpkgs {
              inherit system;
              overlays = [ (import rust-overlay) ];
            };
            treefmt-eval = treefmt-nix.lib.evalModule pkgs ./nix/treefmt.nix;
          }
        );
    in
    {
      packages = forAllSystems (
        { pkgs, ... }:
        let
          trollshell = pkgs.callPackage ./nix/package.nix { };
        in
        {
          inherit trollshell;
          default = trollshell;
        }
      );

      devShells = forAllSystems (
        { pkgs, ... }:
        {
          default = import ./nix/devshell.nix {
            inherit pkgs;
            trollshell = self.packages.${pkgs.stdenv.hostPlatform.system}.trollshell;
          };
        }
      );

      formatter = forAllSystems ({ treefmt-eval, ... }: treefmt-eval.config.build.wrapper);

      checks = forAllSystems (
        { treefmt-eval, ... }:
        {
          formatting = treefmt-eval.config.build.check self;
        }
      );

      nixosModules.default = import ./nix/nixos-module.nix self;
    };
}
