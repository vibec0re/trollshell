{
  description = "trollshell — hytte-based Wayland desktop shell";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    treefmt-nix = {
      url = "github:numtide/treefmt-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
    # Only used by the flake checks (hm-module) to evaluate homeModules.default
    # against a real home-manager module set; not a runtime dependency.
    home-manager = {
      url = "github:nix-community/home-manager";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      treefmt-nix,
      crane,
      home-manager,
      ...
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      # The 12 bundled widget plugins (#558), by crate = binary = flake-output
      # name. Each is packaged by nix/plugin.nix, which since #572 is a plain
      # `cp` of one already-compiled binary out of the single whole-workspace
      # compile (`trollshell.passthru.workspace`) — no cargo, no crane. Shared
      # between the `packages` output (per-plugin flake outputs, so
      # `programs.trollshell.plugins.<id>.package` has something in THIS flake to
      # point at) and the `checks` output (build coverage, #449), so the list
      # lives once. `hytte-plugin-proto` (the wire-protocol lib) and
      # `hytte-plugin` (the SDK) are not plugins and are deliberately absent.
      bundledPluginNames = [
        "hytte-plugin-audio-widget"
        "hytte-plugin-bar-clock-demo"
        "hytte-plugin-caw"
        "hytte-plugin-clock-demo"
        "hytte-plugin-departures"
        "hytte-plugin-infobroker"
        "hytte-plugin-pet"
        "hytte-plugin-preem-demo"
        "hytte-plugin-terminal"
        "hytte-plugin-timer"
        "hytte-plugin-usage"
        "hytte-plugin-weather"
      ];
      # The source revision this build came from (#601), threaded into the
      # *wrapped* binaries' runtime environment as `TROLLSHELL_REV` so a running
      # shell can answer "which commit am I?" — the question that has now cost
      # two rounds of investigating already-fixed behaviour (#375, #566).
      #
      # `self.shortRev` exists only on a CLEAN git tree; a dirty working tree
      # instead carries `self.dirtyShortRev` (e.g. "34e3d96-dirty"), and a
      # non-git source (a `path:` flake, a tarball) has neither — hence the
      # literal fallback. Verified against nix 2.34: the two attributes are
      # mutually exclusive, never both present.
      #
      # Deliberately NOT a compile-time env on the `workspace` derivation: that
      # would change the one expensive crane compile's hash on every commit and
      # invalidate the artifact every package output slices from (see
      # nix/package.nix). It is injected only by the cheap wrapper slices
      # (nix/package.nix's `preFixup`, nix/control-center.nix), which are a `cp`
      # plus a makeWrapper call. The Rust side (trollshell/src/revision.rs)
      # reads it at runtime and falls back to "dev" when unset, which is exactly
      # what a plain `cargo run` gets.
      revision = self.shortRev or (self.dirtyShortRev or "unknown");
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
          trollshell = pkgs.callPackage ./nix/package.nix { inherit craneLib revision; };
          # The single whole-workspace compile (#572). EVERY package output
          # below is a slice of this one derivation — a `cp` of one binary out
          # of its `$out/bin`, optionally wrapped — so there is exactly one
          # cargo invocation on the package path and nothing that can miss a
          # cross-derivation artifact cache. See nix/package.nix.
          workspace = trollshell.passthru.workspace;

          # The control-center companion app (#399): the workspace binary, GApps-
          # wrapped, plus a .desktop launcher. No cargo (#572).
          trollshell-control-center = pkgs.callPackage ./nix/control-center.nix {
            inherit workspace revision;
          };

          # Per-plugin flake packages (#558): `packages.hytte-plugin-<id>` for
          # each of the 12 bundled plugins. Generated from `bundledPluginNames`
          # (one attr each) rather than hand-written. Since #572 each is a `cp`
          # of one already-compiled binary out of `workspace` — no cargo, no
          # crane, no recompile.
          bundledPlugins = pkgs.lib.genAttrs bundledPluginNames (
            name: pkgs.callPackage ./nix/plugin.nix { inherit workspace name; }
          );

          # The `hytte-infobroker` CLI (#562): the #487 consent-gated broker's
          # second `[[bin]]` (crates/hytte-plugin-infobroker/Cargo.toml — a tool,
          # not a widget plugin), used by the etc/skills/infobroker agent-bridge
          # skill. Packaged via the same nix/plugin.nix derivation as the bundled
          # plugins above, but deliberately kept OUT of `bundledPluginNames` —
          # nothing in `programs.trollshell.plugins` should point at it; install
          # it with a plain `home.packages` entry instead (see the skill docs).
          hytte-infobroker = pkgs.callPackage ./nix/plugin.nix {
            inherit workspace;
            name = "hytte-infobroker";
            description = "trollshell consent-gated agent-bridge broker CLI (#487)";
          };

          # The `hytte-claude-bridge` daemon (#584): a keyless loopback shim
          # putting an OpenAI-compatible face on headless Claude Code. Not a
          # widget plugin and not driven by `programs.trollshell.plugins` — it's
          # a standalone daemon behind `etc/systemd/user/trollshell-claude-
          # bridge.service`, so it's kept out of `bundledPluginNames` for the
          # same reason `hytte-infobroker` is. GTK-free, so nix/plugin.nix's
          # unwrapped `cp` is exactly right; no wrapGAppsHook4 needed.
          hytte-claude-bridge = pkgs.callPackage ./nix/plugin.nix {
            inherit workspace;
            name = "hytte-claude-bridge";
            description = "trollshell keyless loopback OpenAI-compatible bridge to headless Claude Code (#584)";
          };

          # Autogenerated `programs.trollshell.*` options reference (#533) +
          # the plugin env-knob reference (#573/#614), both rendered onto one
          # docs site. Its own file (nix/options-doc.nix) — see there for the
          # full rationale — so the `checks` output below can build it too
          # (#614) via the same `pkgs.callPackage` call rather than
          # duplicating the derivation body. Build + read with:
          #   nix build .#options-doc
          #   $BROWSER result/share/doc/trollshell/index.html
          options-doc = pkgs.callPackage ./nix/options-doc.nix { inherit self pkgs; };
        in
        {
          inherit
            trollshell
            trollshell-control-center
            options-doc
            hytte-infobroker
            hytte-claude-bridge
            ;
          default = trollshell;
        }
        // bundledPlugins
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
        {
          pkgs,
          treefmt-eval,
          craneLib,
          ...
        }:
        let
          system = pkgs.stdenv.hostPlatform.system;
          trollshell = pkgs.callPackage ./nix/package.nix { inherit craneLib revision; };
          # The single whole-workspace compile (#572), mirroring the `packages`
          # output above.
          workspace = trollshell.passthru.workspace;

          # The control-center companion app (#411), mirroring the `packages`
          # output above — a slice of `workspace`, no cargo of its own (#572).
          trollshell-control-center = pkgs.callPackage ./nix/control-center.nix {
            inherit workspace revision;
          };

          # The 12 per-plugin packages (#558), mirroring the `packages` output.
          # Merged into `checks` below so `nix flake check` actually *builds*
          # each one — the same reason #449 wired the two existing packages into
          # checks: flake check only builds what's listed here, so without this a
          # broken plugin package could stay green until someone ran `nix build
          # .#hytte-plugin-<id>`. Genuinely near-free since #572: each is a `cp`
          # out of the one `workspace` output every other check already forces.
          bundledPlugins = pkgs.lib.genAttrs bundledPluginNames (
            name: pkgs.callPackage ./nix/plugin.nix { inherit workspace name; }
          );

          # The `hytte-infobroker` CLI package (#562), mirroring the `packages`
          # output above. Wired into `checks` below for the same #449 reason as
          # `bundledPlugins`: without it, `nix flake check` could stay green
          # while `nix build .#hytte-infobroker` was actually broken.
          hytte-infobroker = pkgs.callPackage ./nix/plugin.nix {
            inherit workspace;
            name = "hytte-infobroker";
            description = "trollshell consent-gated agent-bridge broker CLI (#487)";
          };

          # The `hytte-claude-bridge` daemon package (#584), mirroring the
          # `packages` output above and wired into `checks` below for the same
          # #449 reason: without it, `nix flake check` could stay green while
          # `nix build .#hytte-claude-bridge` was broken. Until #757 it was also
          # the one place crane's *git*-dependency vendoring got exercised in CI:
          # `hive-claude` was a rev pin that `builtins.fetchGit` resolved at
          # eval, which a sandboxed build phase could not have done. It comes
          # from crates.io now, so nothing in this flake reaches a third-party
          # forge to evaluate any more (#671).
          hytte-claude-bridge = pkgs.callPackage ./nix/plugin.nix {
            inherit workspace;
            name = "hytte-claude-bridge";
            description = "trollshell keyless loopback OpenAI-compatible bridge to headless Claude Code (#584)";
          };

          # A cheap stand-in for the real trollshell package so the module-eval
          # checks don't force a full Rust crate build just to type-check the
          # config bodies. It carries meta.mainProgram so `lib.getExe cfg.package`
          # (used by the systemd ExecStart) still resolves.
          stubPackage = pkgs.writeShellScriptBin "trollshell" "";

          # Stand-in plugin binary for the programs.trollshell.plugins
          # coverage in the two module-eval checks below (#350/#355).
          stubPlugin = pkgs.writeShellScriptBin "hytte-plugin-demo" "";

          # Stand-ins for the two LLM backend daemons (#694), so the hm-module
          # check below can turn both units on and force their bodies without
          # pulling a full workspace compile (hytte-claude-bridge) or llama-cpp
          # into the check's closure. Named after the binary each unit actually
          # invokes, so `lib.getExe` (bridge, via meta.mainProgram) and
          # `lib.getExe' … "llama-server"` (pet brain, by name) both resolve.
          stubClaudeBridge = pkgs.writeShellScriptBin "hytte-claude-bridge" "";
          stubLlamaCpp = pkgs.writeShellScriptBin "llama-server" "";

          # The hytte-ecal `probe` example binary + fixture sources (a
          # task-list and a calendar), for the eds-nixos-test below. Since #588
          # this is a slice of `workspace` — a `cp` + a GApps wrap, no cargo and
          # no crane — exactly like the plugin packages above. Before #588 it
          # was its own `buildDepsOnly` + `buildPackage` pair with its own `src`
          # filter, i.e. a second full dependency compile per cold flake check.
          probe = pkgs.callPackage ./nix/probe.nix { inherit workspace; };
          # The hytte-services `wifi_probe` example binary, for the
          # wifi-nm-nixos-test below. Same slice treatment (#588) — it was the
          # third full dependency compile.
          wifiProbe = pkgs.callPackage ./nix/wifi-probe.nix { inherit workspace; };
          taskSource = pkgs.writeText "test-tasks.source" ''
            [Data Source]
            DisplayName=Test Tasks
            Enabled=true

            [Task List]
            BackendName=local
          '';
          # A writable local calendar the probe seeds FREQ=DAILY;COUNT=5
          # VEVENTs into, then expands via generate_instances — exercising the
          # RRULE-expansion path (#29) and the EXDATE recurrence-set modifier
          # (the #29 follow-up: one series cancels a day via EXDATE).
          calSource = pkgs.writeText "test-calendar.source" ''
            [Data Source]
            DisplayName=Test Calendar
            Enabled=true

            [Calendar]
            BackendName=local
          '';
        in
        {
          formatting = treefmt-eval.config.build.check self;

          # `options-doc` (#533/#614): pure eval plus one tiny C binary
          # (cmark-gfm) — no crane, nothing expensive — so there's no cost
          # excuse for `options-doc.yml`'s actual render+copy pipeline to go
          # unbuilt on every PR. Mirrors the `packages` output's binding (its
          # own `let`, hence the separate `pkgs.callPackage` call rather than
          # an `inherit` — same reason `trollshell` etc. below are
          # recomputed here too instead of reused across outputs).
          options-doc = pkgs.callPackage ./nix/options-doc.nix { inherit self pkgs; };

          # `nix flake check` only *builds* the derivations listed in `checks`
          # — it does not build `packages` just because they're evaluable.
          # Without these entries, CI (which runs flake check, not
          # `nix build`) can stay green while `nix build .#trollshell` or
          # `.#trollshell-control-center` is actually broken — the release
          # profile, `nix/package.nix`'s src filter, the assets derivation,
          # and the wrapper derivations are never exercised. See #449. Same
          # argument extends to `options-doc` (#614, bound above — already
          # in this attrset, so it isn't repeated in this `inherit`): without
          # it here, a build break in it (a bad cmark-gfm flag, a quoting
          # bug) stays green and first shows up as a red Pages deploy after
          # merge.
          inherit
            trollshell
            trollshell-control-center
            hytte-infobroker
            hytte-claude-bridge
            ;

          # Lint the entire workspace with pedantic-clean Clippy. Reuses
          # cargoArtifacts from the package build so dependencies aren't
          # recompiled from scratch. Must stay green because the workspace
          # denies clippy::all + clippy::pedantic and forbids unsafe.
          #
          # `--features system-tests` pulls the whole-file-gated integration
          # tests (crates/hytte-{bus,reactive,ui}/tests/*.rs) and the gated
          # `mod` blocks (hytte-ui's widget_tests/gtk_tests) into the lint
          # pass too — without it they're invisible to clippy, the same gap
          # #232 found for `cargo test` itself. Verified clean locally
          # (`cargo clippy --workspace --all-targets --features system-tests
          # -- -D warnings`) before wiring this in.
          clippy = craneLib.cargoClippy (
            trollshell.passthru.commonArgs
            // {
              cargoArtifacts = trollshell.passthru.cargoArtifacts;
              # `commonArgs.cargoExtraArgs` already carries `--workspace
              # --locked` (#572), so only the lint-specific flags go here; the
              # effective command is unchanged.
              cargoClippyExtraArgs = "--all-targets --features system-tests -- -D warnings";
              # This is a leaf/terminal check — nothing chains off its target
              # dir as `cargoArtifacts` — so don't pack it. crane defaults
              # `doInstallCargoArtifacts = true`, which would tar the whole
              # (multi-GiB) target dir into $out for no consumer, burning build
              # time and disk. Same fix + reason as system-tests below.
              doInstallCargoArtifacts = false;
            }
          );

          # Run the `system-tests` cargo-feature bucket (#232): the
          # whole-file-`#![cfg(feature = "system-tests")]` integration tests
          # in hytte-bus/hytte-reactive/hytte-ui, plus the `#[cfg(all(test,
          # feature = "system-tests"))]` GTK unit-test modules in hytte-ui.
          # These never compile anywhere else — the workspace compile's own
          # `doCheck` (nix/package.nix) deliberately omits the feature to stay
          # hermetic — so this is their only home. Built via
          # `mkCargoDerivation` directly
          # (rather than `craneLib.cargoTest`) because `cargoTest.nix`
          # hardcodes `checkPhaseCargoCommand`, silently discarding any
          # override — we need that command to wrap `cargo test` in
          # `xvfb-run` for the GTK tests (hytte-ui's `app_smoke`/`bind`/
          # widget-tree & multi-sparkline tests) to have a display.
          # hytte-bus's tests spawn their own ephemeral `dbus-daemon`
          # (crates/hytte-bus/tests/common/mod.rs), so that binary needs to
          # be on PATH too — neither it nor `xvfb-run` are in the package's
          # buildInputs, so both are supplied explicitly here. (Also in
          # nix/devshell.nix's `packages`, #684, so the same command works
          # locally — but this sandboxed check never sees the devShell.)
          # Reuses the same cargoArtifacts as the package build/clippy: the
          # `system-tests` feature is `[]` (no extra deps), so the cached
          # dependency graph is unaffected — only the workspace members
          # themselves (not covered by cargoArtifacts, which only caches
          # true external deps) need recompiling against the extra feature.
          system-tests = craneLib.mkCargoDerivation (
            trollshell.passthru.commonArgs
            // {
              pnameSuffix = "-system-tests";
              cargoArtifacts = trollshell.passthru.cargoArtifacts;
              nativeCheckInputs = [
                pkgs.dbus
                pkgs.xvfb-run
              ];
              doCheck = true;
              # Leaf/terminal check: nothing consumes its target dir. crane
              # defaults `doInstallCargoArtifacts = true`, which packs the whole
              # ~2.2GiB target dir into a `target.tar.zst` — and that pack step
              # was OOM-ing CI's disk ("No space left on device" / "zstd: error
              # 70" in the artifact-install after every test already passed),
              # systematically failing PRs on runners with tight disks. Turning
              # it off stops producing the tarball entirely (#530: less artifact
              # churn overall).
              doInstallCargoArtifacts = false;
              # No separate build step: `cargo test` compiles as part of the
              # check phase. `commonArgs.preBuild` (the libspa-sys writable-
              # vendor-dir workaround) still runs first via the standard
              # (now-empty) buildPhase, same as it does for the `clippy` and
              # `cargoTest`-shaped checks — so the check phase's compile
              # inherits a writable vendor dir.
              buildPhaseCargoCommand = "";
              # A fresh writable $HOME: GTK/glib want to write font/icon
              # caches, and default stdenv HOME is deliberately unwritable.
              # xvfb-run allocates its own virtual display, so no manual
              # Xvfb/DISPLAY wiring is needed. Call the real `cargo` binary
              # directly (not the `cargoWithProfile` shell helper) because
              # xvfb-run execs its argv directly rather than through a
              # shell, so a bash *function* wouldn't resolve — the plain
              # `cargo` binary is on PATH via mkCargoDerivation's own
              # nativeBuildInputs and env vars (CARGO_HOME, vendoring) are
              # inherited by the child process either way.
              preCheck = ''
                export HOME="$(mktemp -d)"
              '';
              checkPhaseCargoCommand = ''
                xvfb-run -a cargo test --workspace --locked --features system-tests
              '';
            }
          );

          # Evaluate homeModules.default against a real home-manager module set so
          # the config bodies (systemd user units, session vars, the swaybg gate,
          # the awww assertion) are actually forced — not just parsed. Builds a
          # trivial derivation that deepSeq's the config attrs that hold those
          # bodies, so a broken body fails the check rather than silently lurking.
          hm-module =
            let
              hm = home-manager.lib.homeManagerConfiguration {
                inherit pkgs;
                modules = [
                  self.homeModules.default
                  {
                    home = {
                      username = "alice";
                      homeDirectory = "/home/alice";
                      stateVersion = "24.11";
                      # `nixpkgs.follows = "nixpkgs"` means HM and nixpkgs ride
                      # the same unstable channel but report different release
                      # numbers; the check is module-eval coverage, not a
                      # release-matched deployment, so silence the warning.
                      enableNixpkgsReleaseCheck = false;
                    };
                    programs.trollshell = {
                      enable = true;
                      package = stubPackage;
                      enableSessionExtras = true;
                      weather.fallbackCity = "Berlin";
                      systemd.target = "niri-session.target";
                      # The two LLM backend units (#694). Both on, so their unit
                      # bodies are forced rather than left behind an unevaluated
                      # `lib.mkIf false`, and so the bridge's timeout-ordering
                      # assertion is *constructed* (it lives inside the
                      # claudeBridge mkIf, which the note on the nixos-module
                      # check below explains would otherwise be invisible).
                      claudeBridge = {
                        enable = true;
                        package = stubClaudeBridge;
                        model = "claude-haiku-4-5";
                        # 15 < the pet's 20 below: the ordering invariant the
                        # module asserts, exercised with a raised client budget
                        # rather than the compiled 10s fallback.
                        timeoutSeconds = 15;
                      };
                      petBrain = {
                        enable = true;
                        package = stubLlamaCpp;
                        model = "/var/empty/brain.gguf";
                      };
                      # plugins (#350/#355, attrsOf keyed by id): `demo` gets a
                      # unit; `off` must be filtered out by enable = false.
                      plugins = {
                        demo = {
                          package = stubPlugin;
                          env.DEMO_TOKEN = "hunter2";
                        };
                        off = {
                          package = stubPlugin;
                          enable = false;
                        };
                        # The client half of the bridge's timeout invariant
                        # (#694): a declared `pet` is what switches that
                        # assertion from vacuously true to a real comparison,
                        # and PET_LLM_TIMEOUT_SECS is the string the module has
                        # to parse the way the plugin does (#699/#711).
                        pet = {
                          package = stubPlugin;
                          env = {
                            PET_LLM_URL = "http://127.0.0.1:8787";
                            PET_LLM_TIMEOUT_SECS = "20";
                          };
                        };
                      };
                    };
                  }
                  # A second module contributing to the *same* plugin key —
                  # attrsOf submodules must merge per-field across modules
                  # (the point of #355), not conflict or drop an entry.
                  { programs.trollshell.plugins.demo.env.DEMO_EXTRA = "1"; }
                ];
              };
              cfg = hm.config;
              # Force the module's config bodies: the systemd user units (incl.
              # the swaybg unit the extras bundle starts under the default
              # backend), the session vars, and every assertion's *predicate* (so
              # the awww-channel assertion in hm-module.nix actually runs). Only
              # the predicates, not the messages — a message is the lazy
              # explanation shown when an assertion fails, so forcing it would be
              # both pointless and prone to evaluating intentionally-deferred text.
              units = cfg.systemd.user.services;
              # The declarative plugin launch state (#419): the module renders
              # the plugins option to trollshell/plugins.json — read back here
              # from the generated file's text — instead of emitting units.
              # fromJSON forbids string context (the exec store path), and this
              # probe only inspects the eval, so discarding it is sound here.
              pluginsState = builtins.fromJSON (
                builtins.unsafeDiscardStringContext cfg.xdg.configFile."trollshell/plugins.json".text
              );
              probe =
                # plugins (#419): entries render to the launch-state file, not
                # static units; enable = false is *declared disabled* (listed,
                # not auto-launched) rather than filtered out; and the demo
                # entry's env still merges per-field across the two modules
                # above (#355).
                assert !(units ? trollshell-plugin-demo);
                assert !(units ? trollshell-plugin-off);
                assert pluginsState.plugins.demo.exec == pkgs.lib.getExe stubPlugin;
                assert pluginsState.plugins.demo.enabled;
                assert pluginsState.plugins.demo.env.DEMO_TOKEN == "hunter2";
                assert pluginsState.plugins.demo.env.DEMO_EXTRA == "1";
                assert !pluginsState.plugins.off.enabled;
                # The two LLM backend units (#694) render, and the bridge keeps
                # the load-bearing bits of etc/systemd/user/trollshell-claude-
                # bridge.service: the four-variable scrub that stops `claude`
                # being silently moved onto metered credits, and the port/model
                # the options are there to set. Asserted by content, not just by
                # membership — a unit that renders without UnsetEnvironment is
                # exactly the regression worth failing the build over.
                assert units ? trollshell-claude-bridge;
                assert units ? trollshell-pet-brain;
                # `builtins.toString` because home-manager's unitOption merge
                # hands some of these back list-wrapped (ExecStart below is
                # `[ "…" ]`, not `"…"`) — toString is identity on a plain string
                # and space-joins the one-element list, so the assertions hold
                # whichever shape the merge produces.
                assert
                  builtins.toString units.trollshell-claude-bridge.Service.UnsetEnvironment
                  == "ANTHROPIC_API_KEY ANTHROPIC_AUTH_TOKEN CLAUDE_CODE_USE_BEDROCK CLAUDE_CODE_USE_VERTEX";
                assert builtins.elem "CLAUDE_BRIDGE_PORT=8787" units.trollshell-claude-bridge.Service.Environment;
                assert builtins.elem "CLAUDE_BRIDGE_MODEL=claude-haiku-4-5"
                  units.trollshell-claude-bridge.Service.Environment;
                assert builtins.elem "CLAUDE_BRIDGE_TIMEOUT_SECS=15"
                  units.trollshell-claude-bridge.Service.Environment;
                # petBrain.model is a runtime path, so it gates the unit rather
                # than entering the closure, and lands in llama-server's argv.
                assert
                  builtins.toString units.trollshell-pet-brain.Unit.ConditionPathExists == "/var/empty/brain.gguf";
                assert
                  let
                    exec = builtins.toString units.trollshell-pet-brain.Service.ExecStart;
                  in
                  pkgs.lib.hasInfix "--model /var/empty/brain.gguf" exec
                  && pkgs.lib.hasInfix "--port 8080" exec
                  && pkgs.lib.hasSuffix "--ctx-size 1024 --threads 4" exec;
                builtins.deepSeq {
                  userUnits = units;
                  sessionVars = cfg.home.sessionVariables;
                  assertionPredicates = map (a: a.assertion) cfg.assertions;
                } "ok";
            in
            pkgs.runCommand "trollshell-hm-module-check" { inherit probe; } ''
              echo "$probe" >/dev/null
              touch $out
            '';

          # Evaluate nixosModules.default the same way. Forces the module's own
          # config contributions — the swaybg/polkit user units, the session
          # vars, and the assertions — rather than system.build.toplevel, to keep
          # it light. A blanket `deepSeq` over the whole NixOS config blows the
          # call stack (systemPackages/systemd recurse through the full package
          # closure), so we force only the trollshell-relevant slices: attrNames
          # of the service sets (which still forces every mkIf/mkMerge branch to
          # decide membership) plus the swaybg unit's ExecStart string (the actual
          # body logic) and the assertions.
          #
          # Note (#681): this only covers assertions the fixture below actually
          # triggers. An assertion behind `lib.mkIf <someOption>` (like the
          # night-light one in nix/nixos-module.nix) only enters `cfg.assertions`
          # once the fixture sets that option — otherwise it's never even
          # constructed, so this check would stay green whether it were correct,
          # inverted, or deleted. See nixos-module-nightlight below, which exists
          # solely to give that conditional assertion a fixture that sets the
          # option it depends on. A new conditional assertion needs the same
          # treatment to be covered at all — don't assume this check already
          # sees it.
          nixos-module =
            let
              nixos = nixpkgs.lib.nixosSystem {
                inherit system;
                modules = [
                  self.nixosModules.default
                  {
                    programs.trollshell = {
                      enable = true;
                      package = stubPackage;
                      weather.fallbackCity = "Berlin";
                      # plugins (#350/#355, attrsOf keyed by id): `demo` gets a
                      # unit; `off` must be filtered out by enable = false.
                      plugins = {
                        demo = {
                          package = stubPlugin;
                          env.DEMO_TOKEN = "hunter2";
                        };
                        off = {
                          package = stubPlugin;
                          enable = false;
                        };
                      };
                    };
                    # Minimal stubs so the NixOS module set evaluates without a
                    # real machine: a bootloader, a root filesystem, and a state
                    # version. nixpkgs.hostPlatform is set via nixosSystem above.
                    boot.loader.grub.enable = false;
                    fileSystems."/" = {
                      device = "/dev/sda1";
                      fsType = "ext4";
                    };
                    system.stateVersion = "24.11";
                  }
                  # A second module contributing to the *same* plugin key —
                  # attrsOf submodules must merge per-field across modules
                  # (the point of #355), not conflict or drop an entry.
                  { programs.trollshell.plugins.demo.env.DEMO_EXTRA = "1"; }
                ];
              };
              cfg = nixos.config;
              # Force the trollshell module's own outputs without descending into
              # package store closures: the unit/var/package key sets, the swaybg
              # ExecStart body, the session vars, the dbus policy package count,
              # and every assertion's boolean. We force only the assertion
              # *predicates*, not their messages — NixOS ships assertions whose
              # `message` is lazy and only well-defined when the assertion fails
              # (e.g. the fileSystems topological-sort error), so deepSeq'ing all
              # messages would trip an unrelated internal assertion's message.
              units = cfg.systemd.user.services;
              # The declarative plugin launch state (#419): the NixOS module
              # renders the plugins option to /etc/xdg/trollshell/plugins.json
              # (the $XDG_CONFIG_DIRS fallback the shell's launcher reads).
              # fromJSON forbids string context (the exec store path), and this
              # probe only inspects the eval, so discarding it is sound here.
              pluginsState = builtins.fromJSON (
                builtins.unsafeDiscardStringContext cfg.environment.etc."xdg/trollshell/plugins.json".text
              );
              probe =
                # plugins (#419): entries render to the launch-state file, not
                # static units; enable = false is *declared disabled* (listed,
                # not auto-launched) rather than filtered out; and the demo
                # entry's env still merges per-field across the two modules
                # above (#355).
                assert !(units ? trollshell-plugin-demo);
                assert !(units ? trollshell-plugin-off);
                assert pluginsState.plugins.demo.exec == pkgs.lib.getExe stubPlugin;
                assert pluginsState.plugins.demo.enabled;
                assert pluginsState.plugins.demo.env.DEMO_TOKEN == "hunter2";
                assert pluginsState.plugins.demo.env.DEMO_EXTRA == "1";
                assert !pluginsState.plugins.off.enabled;
                builtins.deepSeq {
                  userUnits = builtins.attrNames units;
                  swaybgExec = units.swaybg.serviceConfig.ExecStart;
                  sessionVars = cfg.environment.sessionVariables;
                  systemPackageCount = builtins.length cfg.environment.systemPackages;
                  dbusPackageCount = builtins.length cfg.services.dbus.packages;
                  assertionPredicates = map (a: a.assertion) cfg.assertions;
                } "ok";
            in
            pkgs.runCommand "trollshell-nixos-module-check" { inherit probe; } ''
              echo "$probe" >/dev/null
              touch $out
            '';

          # #681: a *separate* nixosModules.default evaluation, deliberately
          # not folded into the fixture above. The check above asserts every
          # predicate is true, which is a useful invariant on its own; this one
          # exists to assert the opposite — that setting a home-manager-only
          # nightlight.* option makes exactly one predicate false — and
          # weakening the first fixture to also cover that would lose the
          # all-true guarantee. This is nix/nixos-module.nix's `nlConfigured`
          # assertion (#657/#680), committed here as the fixture case its
          # author validated by hand with a standalone `nix-instantiate`
          # probe that had nowhere in the repo to live.
          nixos-module-nightlight =
            let
              nixos = nixpkgs.lib.nixosSystem {
                inherit system;
                modules = [
                  self.nixosModules.default
                  {
                    programs.trollshell = {
                      enable = true;
                      package = stubPackage;
                      weather.fallbackCity = "Berlin";
                      # The one thing this fixture exists to set (#657/#680):
                      # nightlight.* is home-manager-only, so configuring it
                      # through the NixOS module must trip
                      # nix/nixos-module.nix's `nlConfigured` assertion.
                      nightlight.latitude = 52.52;
                    };
                    # Same minimal stubs as the nixos-module fixture above, so
                    # this evaluates without a real machine.
                    boot.loader.grub.enable = false;
                    fileSystems."/" = {
                      device = "/dev/sda1";
                      fsType = "ext4";
                    };
                    system.stateVersion = "24.11";
                  }
                ];
              };
              cfg = nixos.config;
              # Same forcing idiom as the check above — predicates only, not
              # every message (see that check's comment on lazily-invalid
              # messages) — but the shape of what we assert is the mirror
              # image: exactly one predicate must be false here, not all of
              # them true. Deliberately not asserting on the total predicate
              # count (~1385 unconfigured, by hand-count while writing this):
              # that number drifts with every nixpkgs bump and would turn this
              # into a maintenance trap unrelated to what it's meant to guard.
              # Asserting on *what* is false — that it's the night-light one,
              # by message content — is both narrower and more stable.
              falsePredicates = builtins.filter (a: !a.assertion) cfg.assertions;
              probe =
                assert builtins.length falsePredicates == 1;
                assert pkgs.lib.hasInfix "nightlight" (builtins.head falsePredicates).message;
                assert pkgs.lib.hasInfix "home-manager" (builtins.head falsePredicates).message;
                builtins.deepSeq { inherit falsePredicates; } "ok";
            in
            pkgs.runCommand "trollshell-nixos-module-nightlight-check" { inherit probe; } ''
              echo "$probe" >/dev/null
              touch $out
            '';

          # The "lean heavy on nix" counterpart to the Rust ephemeral-EDS
          # harness (#49): boot a real NixOS VM with evolution-data-server
          # configured declaratively, seed a fixture task list + calendar, and
          # run the hytte-ecal probe against it end-to-end. The probe also
          # creates FREQ=DAILY;COUNT=5 VEVENTs (one with an EXDATE) and expands
          # them, so this gates the RRULE-expansion fix for #29 and its
          # EXDATE/RDATE follow-up, plus a TZID=Europe/Berlin event whose
          # absolute instant guards the zoned-time fix (#522). Verified to run
          # under TCG (no KVM needed);
          # GitHub's Linux runners have /dev/kvm for speed.
          eds-nixos-test = pkgs.testers.runNixOSTest {
            name = "eds-nixos-test";
            nodes.machine =
              { ... }:
              {
                users.users.alice = {
                  isNormalUser = true;
                  uid = 1000;
                };
                # Three-line EDS module: installs the package, wires its D-Bus
                # session activation service files, and its systemd user units.
                services.gnome.evolution-data-server.enable = true;
                programs.dconf.enable = true; # EDS GSettings backend
                services.gnome.gnome-keyring.enable = true; # EDS credential store
                environment.systemPackages = [ probe ];
                virtualisation.graphics = false;
              };
            testScript = ''
              machine.wait_for_unit("multi-user.target")

              # Seed the fixture task-list + calendar sources into alice's home.
              machine.succeed("mkdir -p /home/alice/.config/evolution/sources")
              machine.copy_from_host(
                  "${taskSource}",
                  "/home/alice/.config/evolution/sources/test-tasks.source",
              )
              machine.copy_from_host(
                  "${calSource}",
                  "/home/alice/.config/evolution/sources/test-calendar.source",
              )
              machine.succeed("chown -R alice:users /home/alice/.config")
              # Store-copied files land read-only (0444); EDS's source
              # registry rewrites .source files on first open to add runtime
              # keys, which fails ("Permission denied") on a read-only file —
              # benign for the auto-provisioned lists but it left the seeded
              # *calendar* unwritable, so creating an event on it failed. Make
              # the fixtures writable.
              machine.succeed("chmod -R u+w /home/alice/.config/evolution")

              # Bring up alice's user session (creates /run/user/1000/bus).
              machine.succeed("loginctl enable-linger alice")
              machine.wait_for_unit("user@1000.service")
              machine.wait_for_file("/run/user/1000/bus")

              # Run the probe as alice; EDS D-Bus-activates on first connect.
              schemas = "${pkgs.evolution-data-server}/share/gsettings-schemas"
              output = machine.wait_until_succeeds(
                  "su -s /bin/sh alice -c '"
                  + "export DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus; "
                  + "export HOME=/home/alice XDG_RUNTIME_DIR=/run/user/1000; "
                  + "export GSETTINGS_SCHEMA_DIR=$(ls -d " + schemas + "/*/glib-2.0/schemas | head -1); "
                  + "probe'",
                  timeout=180,
              )
              # EDS auto-provisions a default "Personal" list, so the count
              # isn't 1 — assert our seeded fixture is enumerated and the FFI
              # create/remove roundtrip actually worked.
              assert "Test Tasks" in output, output
              assert "created uid: hytte-ecal-probe-1" in output, output
              assert "removed hytte-ecal-probe-1" in output, output

              # Live view push (#33): the probe opens a CalClientView over the
              # task list, then from a *second* client connection (standing in
              # for Endeavour) creates + modifies a task. EDS must push the
              # objects-added/-modified notifications to the view — exercising
              # get_view_sync → view_start → the GObject signal trampoline →
              # the boxed Rust callback, pumped via a private GMainContext. A
              # missing push would bail the probe (so wait_until_succeeds would
              # fail); the explicit count line is the positive signal. The probe
              # watches whichever task list EDS lists first (ordering isn't
              # guaranteed — could be the auto-provisioned "Personal" or our
              # "Test Tasks"), so don't pin the name here.
              assert "watching '" in output, output
              assert "editor created uid: hytte-ecal-live-1" in output, output
              assert "editor modified uid: hytte-ecal-live-1" in output, output
              assert "live view push count:" in output, output
              # At least the create + modify pushes landed (initial population +
              # 2). Parse the final count and assert it advanced past the
              # initial-population baseline. (The match can't be None here — the
              # assert above already required the line — but guard it so the
              # test driver's type checker is satisfied.)
              import re
              m = re.search(r"live view push count: (\d+)", output)
              assert m is not None, output
              push_count = int(m.group(1))
              assert push_count >= 2, f"expected >=2 view pushes, got {push_count}: {output}"

              # Recurrence expansion (#29): the probe seeds a
              # FREQ=DAILY;COUNT=5 VEVENT and expands it over a one-month
              # window. All 5 occurrences must materialise — the whole point
              # of the fix (the old master-only path would surface just 1).
              assert "Test Calendar" in output, output
              assert "created recurring uid:" in output, output
              assert "recurring instance count: 5" in output, output
              assert "removed recurring" in output, output

              # EXDATE exclusion (#29 follow-up): the probe seeds a second
              # FREQ=DAILY;COUNT=5 series with an EXDATE cancelling Jun 3.
              # Correct expansion drops that one occurrence (4, not 5) and the
              # cancelled instant must be absent — exactly the user-visible bug
              # this fix closes (a cancelled standup still showing up).
              assert "created exdate uid:" in output, output
              assert "exdate instance count: 4" in output, output
              assert "exdate cancelled occurrence present: false" in output, output
              assert "removed exdate" in output, output

              # Zoned time (#522): a `DTSTART;TZID=Europe/Berlin:…123000` event
              # (12:30 CEST) round-tripped through EDS must expand to the
              # *absolute* instant 10:30 UTC = start_unix 1784889000 — never
              # 1784896200 (12:30 UTC), the +2h double-shift that surfaced a
              # 12:30 event as 14:30 in the Upcoming list. This is the honest
              # end-to-end guard against the pre-fix bug and against #388
              # regressing in reverse; it exercises the real backend store, not
              # just the hermetic string parser.
              assert "created tzid uid:" in output, output
              assert "tzid instance count: 1" in output, output
              assert "tzid instance start_unix: 1784889000" in output, (
                  "TZID=Europe/Berlin 12:30 must resolve to 10:30 UTC "
                  "(1784889000), not 12:30 UTC (1784896200): " + output
              )
              assert "tzid instance start_unix: 1784896200" not in output, output
              assert "removed tzid" in output, output
            '';
          };

          # The "lean heavy on nix" harness for the NetworkManager Wi-Fi
          # backend (#96): boot a real NixOS VM with NetworkManager and a pair
          # of virtual Wi-Fi radios (mac80211_hwsim), then drive wifi_nm
          # end-to-end via the wifi_probe example — backend detection, device
          # discovery, a live RequestScan, and a state read. mac80211_hwsim
          # gives NM a real (simulated) wlan device so the whole D-Bus path
          # exercises against a live daemon, not a mock. Mirrors
          # eds-nixos-test; runs under TCG (no KVM needed).
          wifi-nm-nixos-test = pkgs.testers.runNixOSTest {
            name = "wifi-nm-nixos-test";
            nodes.machine =
              { ... }:
              {
                networking.networkmanager.enable = true;
                # Two virtual 802.11 radios; NM manages the resulting wlan
                # interfaces, giving the probe a real device + AP scan path.
                boot.kernelModules = [ "mac80211_hwsim" ];
                boot.extraModprobeConfig = "options mac80211_hwsim radios=2";
                environment.systemPackages = [ wifiProbe ];
                virtualisation.graphics = false;
              };
            testScript = ''
              machine.wait_for_unit("multi-user.target")
              machine.wait_for_unit("NetworkManager.service")
              # Wait until NM has a Wi-Fi device registered (hwsim + NM takeover).
              machine.wait_until_succeeds(
                  "nmcli -t -f DEVICE,TYPE device | grep ':wifi'", timeout=60
              )

              # Run the probe as root on the system bus — drives wifi_nm against
              # the live NetworkManager.
              output = machine.wait_until_succeeds("wifi_probe", timeout=180)
              assert "backend=NetworkManager" in output, output
              assert "device=" in output, output
              assert "scan=" in output, output
              assert "networks=" in output, output
            '';
          };
        }
        // bundledPlugins
      );

      # `import … self` (not a bare path) because the module's `package` option
      # defaults to self.packages.<system>.trollshell. A bare-path module would
      # instead build via `pkgs.callPackage ./package.nix`, which needs crane
      # (craneLib) wired into the consumer's nixpkgs; threading `self` reuses the
      # package we already built here rather than pushing that onto consumers.
      nixosModules.default = import ./nix/nixos-module.nix self;

      # Curried the same way and for the same reason as the NixOS module above.
      homeModules.default = import ./nix/hm-module.nix self;
      homeManagerModules.default = self.homeModules.default;
    };
}
