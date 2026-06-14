{
  description = "trollshell — hytte-based Wayland desktop shell";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    treefmt-nix = {
      url = "github:numtide/treefmt-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      self,
      nixpkgs,
      treefmt-nix,
      crane,
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
            pkgs = import nixpkgs { inherit system; };
            # crane drives the build on nixpkgs' own rust toolchain.
            craneLib = crane.mkLib pkgs;
            treefmt-eval = treefmt-nix.lib.evalModule pkgs ./nix/treefmt.nix;
          }
        );
    in
    {
      packages = forAllSystems (
        { pkgs, craneLib, ... }:
        let
          trollshell = pkgs.callPackage ./nix/package.nix { inherit craneLib; };
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

      # `import … self` (not a bare path) because the module's `package`
      # option defaults to self.packages.<system>.trollshell — built against
      # our rust-overlay/crane pkgs. A bare-path module would default via
      # `pkgs.callPackage ./package.nix`, which needs rust-overlay in the
      # consumer's nixpkgs; threading `self` keeps that our problem, not theirs.
      nixosModules.default = import ./nix/nixos-module.nix self;
    };
}
