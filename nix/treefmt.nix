{ lib, ... }:
{
  projectRootFile = "flake.nix";

  programs.nixfmt.enable = true;
  programs.rustfmt = {
    enable = true;
    edition = "2024";
  };
  programs.taplo.enable = true;
  # …but not over a TOML file that is a RECORDING of another tool's output.
  # `crates/hytte-plugin-agents/tests/fixtures/agents-nix-rendered.toml` is
  # pinned byte-for-byte against what `pkgs.formats.toml` renders from
  # `programs.trollshell.config.agents`, by BOTH platform modules
  # (`checks.nixos-module-agents-fixture` / `checks.hm-module-agents-fixture`)
  # and read back through the plugin's real `Subsystem` reader by a Rust test
  # in the same crate — so whoever formats it is no longer taplo, it is nix.
  # Harmless until #1227 gave the file an array (`_locked`): `json2toml` emits
  # one on a single line, taplo wraps it over six, and every one of those
  # checks goes red on a `nix fmt` nobody asked to change behaviour.
  settings.formatter.taplo.excludes = [
    "crates/hytte-plugin-agents/tests/fixtures/*.toml"
  ];

  programs.prettier.enable = true;
  # Only let prettier touch markdown + CSS — its default globs also grab
  # JSON/YAML, which would reformat flake.lock and friends. mkForce replaces the
  # module's broad default rather than appending to it.
  settings.formatter.prettier.includes = lib.mkForce [
    "*.md"
    "*.css"
  ];
}
