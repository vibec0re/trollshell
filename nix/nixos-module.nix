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

  options.programs.trollshell.enableRecommendedServices = lib.mkOption {
    type = lib.types.bool;
    default = true;
    description = ''
      Enable the system daemons trollshell's optional chips lean on (UPower,
      power-profiles-daemon, geoclue, plus the polkit agent and the agent-name
      D-Bus policy). Each is still individually overridable; this is a master
      switch for the lot. Turning it off leaves a working bar — the chips that
      back onto a missing daemon simply hide themselves.
    '';
  };

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

        # Fonts the stylesheets name explicitly: Inter + Cantarell for the bar
        # UI (hytte-ui/src/style.css, trollshell/style.css), JetBrains Mono /
        # Fira Code for the clock + workspace chips. Without them fontconfig
        # silently falls back and the bar renders in the wrong typeface.
        fonts.packages = [
          pkgs.inter
          pkgs.cantarell-fonts
          pkgs.jetbrains-mono
          pkgs.fira-code
        ];

        # Weather widget location fallback. Stays outside the recommended-
        # services switch on purpose: it's the manual alternative to geoclue,
        # so it must keep working when auto-location is turned off. weather
        # forward-geocodes this city when GeoClue2 is absent.
        environment.sessionVariables.TROLLSHELL_WEATHER_CITY = lib.mkIf (
          cfg.weather.fallbackCity != null
        ) cfg.weather.fallbackCity;

        # Wallpaper reload command for the Appearance picker. null = the shell's
        # built-in default (restart swaybg.service); set it to drive swww/awww
        # or another daemon. The chosen path reaches the command as
        # TROLLSHELL_WALLPAPER_PATH.
        environment.sessionVariables.TROLLSHELL_WALLPAPER_RELOAD_CMD = lib.mkIf (
          cfg.wallpaper.reloadCommand != null
        ) cfg.wallpaper.reloadCommand;
      }
      # The recommended-but-optional system daemons trollshell's chips lean
      # on, grouped behind the master switch. Each chip hides itself when its
      # daemon is missing, so dropping the lot still leaves a working bar.
      # Per-daemon mkDefault keeps an explicit `services.<d>.enable = false;`
      # in user config intact even while the switch is on.
      (lib.mkIf cfg.enableRecommendedServices {
        # UPower drives the battery chip + plug/unplug OSDs. Without it the
        # chip stays hidden (BatteryState::Unknown) and the five property
        # subscriptions sit in PropState::Loading forever.
        services.upower.enable = lib.mkDefault true;

        # power-profiles-daemon (net.hadess.PowerProfiles) feeds the
        # power-profile selector. Without it ActiveProfile + Profiles stay in
        # PropState::Loading; the profile group hides itself (panels gate on
        # available.is_empty()) so the drawer stays clean.
        services.power-profiles-daemon.enable = lib.mkDefault true;

        # geoclue2 auto-locates the weather widget (and future location-aware
        # features). Without it the geoclue service times out and falls back to
        # TROLLSHELL_WEATHER_CITY; with neither, weather shows a "set a city"
        # hint rather than breaking. Still individually gated by its own toggle.
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

        # System-bus policy: allow any user to own the two trollshell agent
        # names. BlueZ / iwd policies still gate the actual method ACLs; this
        # only grants the right to RequestName. Without it, hytte_bus::own_name
        # hits AccessDenied at the broker and parks the agent inert with one
        # info-level log (own.rs) — the bluetooth/wifi pairing agents just go
        # quiet, nothing crashes.
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

        # Polkit authentication agent. trollshell no longer ships its own
        # in-process agent; run the standard standalone polkit-gnome agent as a
        # user service bound to the graphical session. Without it the bar still
        # runs — only polkit-mediated privilege prompts lose their GUI. Swap
        # polkit_gnome for another agent (mate-polkit, hyprpolkitagent, …) by
        # overriding this unit's ExecStart.
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
      })
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
