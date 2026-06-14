self:
{
  config,
  options,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.programs.trollshell;
in
{
  # enable / package / weather.fallbackCity are declared in the shared base.
  imports = [ (import ./module-common.nix self) ];

  # geoclue is system-only, so it lives here rather than in the shared base.
  options.programs.trollshell.weather.geoclue = {
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

  config = lib.mkIf cfg.enable (
    lib.mkMerge [
      {
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
      }
      # When the home-manager NixOS module is in use, register trollshell's HM
      # module as a shared module so per-user `programs.trollshell` config is
      # available everywhere (the user service starts once a user also sets
      # programs.trollshell.enable = true in their home-manager config).
      (lib.optionalAttrs (options ? home-manager) {
        home-manager.sharedModules = [ self.homeModules.default ];
      })
    ]
  );
}
