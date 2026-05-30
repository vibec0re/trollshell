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
          adwaita-icon-theme
          hicolor-icon-theme

          evolution-data-server
          libical
          gobject-introspection

          pam

          openssl
        ];

        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };

        trollshell = rustPlatform.buildRustPackage {
          pname = "trollshell";
          version = "0.1.0";
          src = ./.;

          cargoLock = {
            lockFile = ./Cargo.lock;
          };

          # Workspace has multiple binaries; we only need the trollshell one.
          cargoBuildFlags = [ "-p" "trollshell" ];
          # Tests touch live system daemons (dbus, etc.); skip in nix sandbox.
          doCheck = false;

          inherit nativeBuildInputs buildInputs;

          env = {
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
            BINDGEN_EXTRA_CLANG_ARGS = "-I${pkgs.pam}/include -I${pkgs.glibc.dev}/include";
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

          meta = with pkgs.lib; {
            description = "hytte-based Wayland desktop shell";
            homepage = "https://git.hannig.cc/choom/trollshell";
            license = licenses.mpl20;
            platforms = platforms.linux;
            mainProgram = "trollshell";
          };
        };
      in
      {
        packages = {
          inherit trollshell;
          default = trollshell;
        };

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
            # mkShell doesn't export icon-theme share paths into XDG_DATA_DIRS
            # via setup hooks, so GTK's icon loader can't find Adwaita symbolics
            # (audio-volume-*-symbolic, display-brightness-symbolic, etc.).
            # Prepend them explicitly so `cargo run` from the devShell sees them.
            export XDG_DATA_DIRS="${pkgs.adwaita-icon-theme}/share:${pkgs.hicolor-icon-theme}/share:$XDG_DATA_DIRS"

            # Nix packages GSettings schemas under share/gsettings-schemas/<pkg>/...,
            # but GLib only finds them at share/glib-2.0/schemas/. wrapGAppsHook
            # translates this at install time; for `cargo run` we point GLib at
            # the raw schema dirs ourselves so org.gnome.desktop.interface (and
            # therefore the active GTK icon theme name) reads cleanly.
            export GSETTINGS_SCHEMA_DIR="${pkgs.gsettings-desktop-schemas}/share/gsettings-schemas/${pkgs.gsettings-desktop-schemas.name}/glib-2.0/schemas:${pkgs.gtk4}/share/gsettings-schemas/${pkgs.gtk4.name}/glib-2.0/schemas"
          '';
        };

        formatter = pkgs.nixpkgs-fmt;
      }) // {
        nixosModules.default = { config, lib, pkgs, ... }:
          let
            cfg = config.programs.trollshell;
          in
          {
            options.programs.trollshell = {
              enable = lib.mkEnableOption "trollshell — hytte-based Wayland desktop shell";

              package = lib.mkOption {
                type = lib.types.package;
                default = self.packages.${pkgs.system}.trollshell;
                defaultText = lib.literalExpression "trollshell.packages.\${system}.trollshell";
                description = "The trollshell package to install.";
              };
            };

            config = lib.mkIf cfg.enable {
              environment.systemPackages = [ cfg.package ];

              # UPower drives the battery chip + plug/unplug OSDs. Without
              # it, the chip stays hidden (BatteryState::Unknown) and the
              # five property subscriptions sit in PropState::Loading
              # forever. mkDefault leaves explicit `services.upower.enable
              # = false;` in user config intact for the rare desktop case.
              services.upower.enable = lib.mkDefault true;

              # power-profiles-daemon (net.hadess.PowerProfiles) feeds the
              # power-profile selector. Without it, ActiveProfile + Profiles
              # stay in PropState::Loading and the chip can't show or set
              # Performance/Balanced/Power-Saver.
              services.power-profiles-daemon.enable = lib.mkDefault true;

              # System-bus policy: allow any user to own the three trollshell
              # agent names. BlueZ / polkit / iwd policies still gate the
              # actual method ACLs; this only grants the right to RequestName.
              # Without it, hytte_bus::own_name detects AccessDenied at the
              # broker and parks the agent inert with one info-level log.
              services.dbus.packages = [
                (pkgs.writeTextDir "share/dbus-1/system.d/cc.hannig.trollshell.conf" ''
                  <!DOCTYPE busconfig PUBLIC
                    "-//freedesktop//DTD D-BUS Bus Configuration 1.0//EN"
                    "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
                  <busconfig>
                    <policy context="default">
                      <allow own="cc.hannig.trollshell.bluez-agent"/>
                      <allow own="cc.hannig.trollshell.polkit-agent"/>
                      <allow own="cc.hannig.trollshell.iwd-agent"/>
                    </policy>
                  </busconfig>
                '')
              ];
            };
          };
      };
}
