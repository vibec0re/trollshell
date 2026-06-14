{ lib, ... }:
{
  projectRootFile = "flake.nix";

  programs.nixfmt.enable = true;
  programs.rustfmt = {
    enable = true;
    edition = "2024";
  };
  programs.taplo.enable = true;

  programs.prettier.enable = true;
  # Only let prettier touch markdown — its default globs also grab JSON/YAML,
  # which would reformat flake.lock and friends. mkForce replaces the module's
  # broad default rather than appending to it.
  settings.formatter.prettier.includes = lib.mkForce [ "*.md" ];
}
