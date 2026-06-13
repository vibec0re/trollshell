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
        {
          default = import ./nix/devshell.nix {
            inherit pkgs;
            trollshell = self.packages.${pkgs.stdenv.hostPlatform.system}.trollshell;
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
