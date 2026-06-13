{
  description = "trollshell — hytte-based Wayland desktop shell";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
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
          }
        );
    in
    {
      packages = forAllSystems (
        { pkgs }:
        let
          trollshell = pkgs.callPackage ./nix/package.nix { };
        in
        {
          inherit trollshell;
          default = trollshell;
        }
      );

      devShells = forAllSystems (
        { pkgs }:
        let
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

            openssl

            pipewire
          ];
        in
        {
          default = pkgs.mkShell {
            inherit nativeBuildInputs buildInputs;

            packages = with pkgs; [
              rust-analyzer
            ];

            env = {
              LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
              BINDGEN_EXTRA_CLANG_ARGS = "-I${pkgs.glibc.dev}/include";
            };

            shellHook = ''
              export RUST_BACKTRACE=1
              # Put libclang.so on the dynamic loader's search path so the
              # bindgen consumers (libpipewire-sys + libspa-sys) can `dlopen`
              # it. clang-sys's libloading fallback otherwise leans on
              # LIBCLANG_PATH alone, which can race with sibling bindgen
              # invocations in workspace builds.
              export LD_LIBRARY_PATH="$LIBCLANG_PATH:''${LD_LIBRARY_PATH:-}"

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
        }
      );

      formatter = forAllSystems ({ pkgs }: pkgs.nixpkgs-fmt);

      nixosModules.default =
        {
          config,
          lib,
          pkgs,
          ...
        }:
        let
          cfg = config.programs.trollshell;
        in
        {
          options.programs.trollshell = {
            enable = lib.mkEnableOption "trollshell — hytte-based Wayland desktop shell";

            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.stdenv.hostPlatform.system}.trollshell;
              defaultText = lib.literalExpression "trollshell.packages.\${system}.trollshell";
              description = "The trollshell package to install.";
            };

            weather = {
              fallbackCity = lib.mkOption {
                type = lib.types.nullOr lib.types.str;
                default = null;
                example = "Berlin";
                description = ''
                  City the weather widget falls back to when geolocation is
                  unavailable. Sets TROLLSHELL_WEATHER_CITY session-wide.
                  Leave null to rely on geoclue.
                '';
              };

              geoclue = {
                enable = lib.mkOption {
                  type = lib.types.bool;
                  default = true;
                  description = "Enable geoclue2 auto-location for the weather widget (and future location-aware features).";
                };

                providerUrl = lib.mkOption {
                  type = lib.types.str;
                  default = "https://api.beacondb.net/v1/geolocate";
                  description = ''
                    WiFi-positioning backend for geoclue. Mozilla Location
                    Service (geoclue's historical default) shut down in 2024;
                    beaconDB is the community successor.
                  '';
                };
              };
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

            # System-bus policy: allow any user to own the two trollshell
            # agent names. BlueZ / iwd policies still gate the
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
                    <allow own="cc.hannig.trollshell.iwd-agent"/>
                  </policy>
                </busconfig>
              '')
            ];

            # Polkit authentication agent. trollshell no longer ships its
            # own in-process agent; run the standard standalone polkit-gnome
            # agent as a user service bound to the graphical session. Swap
            # polkit_gnome for another agent (mate-polkit, hyprpolkitagent,
            # …) by overriding this unit's ExecStart.
            systemd.user.services.polkit-gnome-authentication-agent-1 = {
              description = "polkit-gnome authentication agent";
              wantedBy = [ "graphical-session.target" ];
              partOf = [ "graphical-session.target" ];
              after = [ "graphical-session.target" ];
              serviceConfig = {
                Type = "simple";
                ExecStart = "${pkgs.polkit_gnome}/libexec/polkit-gnome-authentication-agent-1";
                Restart = "on-failure";
                RestartSec = 1;
                TimeoutStopSec = 10;
              };
            };

            # Weather widget location: a session-wide TROLLSHELL_WEATHER_CITY
            # fallback plus geoclue auto-location (also feeds future
            # location-aware features like departures).
            environment.sessionVariables.TROLLSHELL_WEATHER_CITY = lib.mkIf (
              cfg.weather.fallbackCity != null
            ) cfg.weather.fallbackCity;

            services.geoclue2 = lib.mkIf cfg.weather.geoclue.enable {
              enable = lib.mkDefault true;
              # MLS is dead — point geoclue's wifi backend at beaconDB.
              geoProviderUrl = lib.mkDefault cfg.weather.geoclue.providerUrl;
              # Let trollshell's geoclue client (DesktopId "trollshell")
              # request location.
              appConfig.trollshell = {
                isAllowed = true;
                isSystem = false;
              };
            };
          };
        };
    };
}
