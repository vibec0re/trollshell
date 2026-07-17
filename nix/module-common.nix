# The programs.trollshell options shared by the NixOS and home-manager modules,
# so enable/package/weather.fallbackCity stay declared in one place instead of
# drifting between two near-identical copies. Each module imports this and adds
# its own platform-specific options (geoclue system-side, systemd user service
# home-side) plus the matching config.
self:
{
  config,
  lib,
  pkgs,
  ...
}:
{
  options.programs.trollshell = {
    enable = lib.mkEnableOption "trollshell — hytte-based Wayland desktop shell";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.trollshell;
      defaultText = lib.literalExpression "trollshell.packages.\${system}.trollshell";
      description = "The trollshell package to install.";
    };

    weather.fallbackCity = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "Berlin";
      description = ''
        City the weather widget falls back to when geolocation is unavailable.
        Sets TROLLSHELL_WEATHER_CITY for the session. Leave null to rely on
        geoclue (enabled system-side by the NixOS module).
      '';
    };

    wallpaper.backend = lib.mkOption {
      type = lib.types.enum [
        "swaybg"
        "awww"
        "none"
      ];
      default = "swaybg";
      example = "awww";
      description = ''
        Which wallpaper daemon trollshell manages for the session, and what the
        Appearance picker tells to reload. One enum value drives the daemon
        wiring and the `reloadCommand` default together, so two daemons can never
        run at once — backend selection is structural, not an assertion.

        - `swaybg` (default — today's behavior): the bundled swaybg user unit;
          the Appearance picker restarts it (the shell's built-in default), so
          `reloadCommand` stays null.
        - `awww`: the swww successor (upstream renamed swww → awww at 0.12; swww
          itself is deprecated). Defaults `reloadCommand` to `awww img {}`. The
          daemon is run by home-manager's `services.awww`; the NixOS module only
          exports the reload command (a NixOS-only user without home-manager
          runs the awww daemon themselves).
        - `none`: manage no daemon at all — the Appearance picker only writes
          `~/.config/trollshell/wallpaper.path` (and runs `reloadCommand` if you
          set one yourself). Use this to wire your own daemon.

        The legacy `programs.trollshell.swaybg.enable` (home-manager) and a
        hand-set `wallpaper.reloadCommand` both still work and take precedence;
        they are gently deprecated in favor of this enum.
      '';
    };

    wallpaper.reloadCommand = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      # Per backend (`awww img {}` for awww). An option default, so any explicit
      # `reloadCommand = "…"` the user sets already wins over it (defaults are the
      # lowest merge priority). `{}` is the shell-quoted path, NOT a $VAR.
      default =
        {
          swaybg = null;
          awww = "awww img {}";
          none = null;
        }
        .${config.programs.trollshell.wallpaper.backend};
      defaultText = lib.literalExpression ''# per backend: null (swaybg/none) or "awww img {}" (awww)'';
      example = "awww img {}";
      description = ''
        Shell command the Appearance picker runs (via `sh -c`) after writing
        `~/.config/trollshell/wallpaper.path`, to tell your wallpaper daemon to
        reload. A `{}` in the command is replaced with the chosen path,
        shell-quoted. Sets TROLLSHELL_WALLPAPER_RELOAD_CMD for the session.

        Use the `{}` placeholder, not a `$VAR` reference: this value is delivered
        through sessionVariables, which expands `$`-references at login (before
        the path exists), so `awww img "$TROLLSHELL_WALLPAPER_PATH"` would expand
        to `awww img ""`. The path is still also exported as
        TROLLSHELL_WALLPAPER_PATH for daemons that read it directly.

        Defaults follow `wallpaper.backend`: null for `swaybg` (restart the
        bundled swaybg unit — the shell's built-in default) and `none`, and
        `awww img {}` for `awww`. Set it explicitly to override; an explicit
        value wins over the per-backend default.
      '';
    };

    # Night-light (color temperature) via a wlsunset user unit. The shell only
    # toggles the unit on/off (nightlight.rs, mirroring the wallpaper picker's
    # zero-state daemon playbook); these options parameterise the unit's
    # ExecStart. The unit itself is declared home-manager-side (nix/hm-module.nix)
    # alongside the swaybg unit — it is a per-user wlr-gamma-control daemon, not a
    # system service. lat/lon default to null (unset); the unit stays inert until
    # both are set. geoclue lat/lon seeding is a deferred follow-up (v1 is static
    # coordinates only).
    nightlight = {
      latitude = lib.mkOption {
        type = lib.types.nullOr (lib.types.either lib.types.float lib.types.str);
        default = null;
        example = 52.52;
        description = ''
          Latitude in decimal degrees for wlsunset's geo (sunrise/sunset) mode
          (`wlsunset -l <latitude>`). Required — together with `longitude` — for
          the Night light toggle to do anything; while either is null the
          `wlsunset.service` unit is declared but inert. May be given as a float
          (52.52) or a string ("52.52"); a string avoids the trailing zeros nix
          renders for floats.
        '';
      };

      longitude = lib.mkOption {
        type = lib.types.nullOr (lib.types.either lib.types.float lib.types.str);
        default = null;
        example = 13.405;
        description = ''
          Longitude in decimal degrees for wlsunset's geo mode
          (`wlsunset -L <longitude>`). Required — together with `latitude` — for
          the Night light toggle to do anything. May be a float or a string (see
          `latitude`).
        '';
      };

      dayTemp = lib.mkOption {
        type = lib.types.int;
        default = 6500;
        example = 6500;
        description = ''
          Daytime color temperature in kelvin (`wlsunset -T <dayTemp>`). 6500K is
          neutral daylight (wlsunset's own default).
        '';
      };

      nightTemp = lib.mkOption {
        type = lib.types.int;
        default = 4000;
        example = 3500;
        description = ''
          Nighttime color temperature in kelvin (`wlsunset -t <nightTemp>`).
          Lower is warmer; 4000K is a gentle warm-white.
        '';
      };
    };

    plugins = lib.mkOption {
      type = lib.types.attrsOf (
        lib.types.submodule {
          options = {
            enable = lib.mkOption {
              type = lib.types.bool;
              default = true;
              example = false;
              description = ''
                Whether to generate the systemd user service for this
                plugin. Lets a downstream module switch a single entry off
                (`plugins.pet.enable = false;`) without removing its
                definition.
              '';
            };

            package = lib.mkOption {
              type = lib.types.package;
              description = ''
                The out-of-tree plugin's package (its out-of-process binary,
                built on hytte-plugin). It dials plugin.sock and speaks the
                Register handshake itself — the shell never spawns or links
                it (trollshell/src/plugins.rs).
              '';
            };

            env = lib.mkOption {
              type = lib.types.attrsOf lib.types.str;
              default = { };
              example = {
                PET_NAME = "nisse";
                PET_LLM_MODEL = "google/gemini-3.5-flash";
              };
              description = ''
                Environment variables passed to the plugin binary — the
                config idiom the bundled plugins already use (PET_NAME,
                PET_LLM_MODEL, …; see etc/systemd/user/trollshell-plugin-pet.service).
                Values must be strings.
              '';
            };
          };
        }
      );
      default = { };
      example = lib.literalExpression ''
        {
          hyperhive = {
            package = pkgs.hyperhive-plugin;
            env.HYPERHIVE_TOKEN = "…";
          };
        }
      '';
      description = ''
        Declarative out-of-tree plugins (#350), keyed by plugin id: each
        attr drops in without hand-writing a systemd unit. One
        `trollshell-plugin-<id>` user service is generated per entry (the
        attr key is the id) — same shape as the bundled
        `etc/systemd/user/trollshell-plugin-pet.service` (bound to
        niri-session.target, started after trollshell.service, restarted
        on-failure). Being an attrset of submodules (the standard
        named-instance pattern), entries merge per-field across modules —
        a second module can override one field of one plugin
        (`plugins.pet.env.PET_NAME = lib.mkForce "nisse";`) or disable it
        (`plugins.pet.enable = false;`). The plugin dials plugin.sock and
        registers itself; this option only wires the unit, it does not
        spawn/supervise the plugin or hot-load it into a running shell
        (that's runtime load/unload, #348 — out of scope here).
      '';
    };
  };
}
