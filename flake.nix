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
      # The bundled widget plugins (#558), by crate = binary = flake-output
      # name. Each is packaged by nix/plugin.nix, which since #572 is a plain
      # `cp` of one already-compiled binary out of the single whole-workspace
      # compile (`trollshell.passthru.workspace`) — no cargo, no crane. Shared
      # between the `packages` output (per-plugin flake outputs, so
      # `programs.trollshell.plugins.<id>.package` has something in THIS flake to
      # point at) and the `checks` output (build coverage, #449), so the list
      # lives once. `hytte-plugin-proto` (the wire-protocol lib) and
      # `hytte-plugin` (the SDK) are not plugins and are deliberately absent.
      bundledPluginNames = [
        "hytte-plugin-agents"
        "hytte-plugin-audio-widget"
        "hytte-plugin-bar-clock-demo"
        "hytte-plugin-caw"
        "hytte-plugin-clock-demo"
        "hytte-plugin-departures"
        "hytte-plugin-infobroker"
        "hytte-plugin-niri-layouts"
        "hytte-plugin-pet"
        "hytte-plugin-preem-demo"
        # One binary meant to be listed twice in `programs.trollshell.plugins`
        # (#1250): a bar instance and a right-sidebar one, distinguished by
        # `HYTTE_PLUGIN_ID` + `HYTTE_PLUGIN_MOUNT` on the launch. That is a
        # deployment shape, not a packaging one — there is still exactly one
        # slice here, and `plugins.<id>.package` points both entries at it.
        "hytte-plugin-stats"
        "hytte-plugin-terminal"
        "hytte-plugin-timer"
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

          # The per-agent companion window (#950, P2 of the agentic desktop):
          # hyperhive's own agent page in a WebKitGTK view inside our chrome.
          # Same slice-and-wrap shape as the control center — it is a windowed
          # GTK app, and WebKit needs the GApplication env — but with no
          # .desktop item, since it is meaningless without `--agent <name>`
          # (nix/agent-window.nix says why). `programs.trollshell.agentWindow`
          # installs it; the agents plugin launches it off `PATH`.
          trollshell-agent-window = pkgs.callPackage ./nix/agent-window.nix {
            inherit workspace revision;
          };

          # Per-plugin flake packages (#558): `packages.hytte-plugin-<id>` for
          # each bundled plugin. Generated from `bundledPluginNames`
          # (one attr each) rather than hand-written. Since #572 each is a `cp`
          # of one already-compiled binary out of `workspace` — no cargo, no
          # crane, no recompile.
          bundledPlugins = pkgs.lib.genAttrs bundledPluginNames (
            name:
            pkgs.callPackage ./nix/plugin.nix (
              {
                inherit workspace name;
              }
              # `hytte-plugin-niri-layouts` is the one bundled plugin with a
              # standalone-CLI hat (`apply <layout>`, #1019) and so the one
              # with a `completions <shell>` subcommand to build (#1116); every
              # other bundled plugin is driven entirely by the wire protocol
              # and has no argv to complete.
              // pkgs.lib.optionalAttrs (name == "hytte-plugin-niri-layouts") {
                hasCompletions = true;
              }
            )
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
            # The CLI's own `completions <shell>` subcommand (#1116).
            hasCompletions = true;
          };

          # The `hytte-claude-bridge` daemon (#584): a keyless same-uid-socket shim
          # putting an OpenAI-compatible face on headless Claude Code. Since #866
          # it speaks the plugin protocol too (a status chip) and IS driven by
          # `programs.trollshell.plugins` — `nix/hm-module.nix` renders the
          # `claude-bridge` entry from `claudeBridge.*`, so nobody points
          # `plugins.<id>.package` at this by hand. It still stays out of
          # `bundledPluginNames`, which is a mechanical `hytte-plugin-<id>` →
          # package map and this binary is not named that way. GTK-free, so
          # nix/plugin.nix's unwrapped `cp` is exactly right; no wrapGAppsHook4
          # needed.
          hytte-claude-bridge = pkgs.callPackage ./nix/plugin.nix {
            inherit workspace;
            name = "hytte-claude-bridge";
            description = "trollshell keyless same-uid-socket OpenAI-compatible bridge to headless Claude Code (#584, #993)";
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
            trollshell-agent-window
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
          # The checks-universe probe-examples compile (#1257) — see
          # nix/package.nix's `probes` binding. `probe`/`wifiProbe` below take
          # BOTH this and `workspace`: the binary from here, the GApps wrap's
          # `buildInputs` from `workspace.passthru.devInputs`.
          probes = trollshell.passthru.probes;

          # The control-center companion app (#411), mirroring the `packages`
          # output above — a slice of `workspace`, no cargo of its own (#572).
          trollshell-control-center = pkgs.callPackage ./nix/control-center.nix {
            inherit workspace revision;
          };

          # The #950 companion window, mirroring the `packages` output. Wired
          # in here — the I1 gap the #1130 review measured — now that #1127 has
          # landed and this region is stable: it is the one output whose
          # **wrapper** derivation (nix/agent-window.nix) nothing else builds,
          # and the wrapper is exactly where M1's closure bug lived.
          trollshell-agent-window = pkgs.callPackage ./nix/agent-window.nix {
            inherit workspace revision;
          };

          # The per-plugin packages (#558), mirroring the `packages` output.
          # Merged into `checks` below so `nix flake check` actually *builds*
          # each one — the same reason #449 wired the two existing packages into
          # checks: flake check only builds what's listed here, so without this a
          # broken plugin package could stay green until someone ran `nix build
          # .#hytte-plugin-<id>`. Genuinely near-free since #572: each is a `cp`
          # out of the one `workspace` output every other check already forces.
          bundledPlugins = pkgs.lib.genAttrs bundledPluginNames (
            name:
            pkgs.callPackage ./nix/plugin.nix (
              {
                inherit workspace name;
              }
              # `hytte-plugin-niri-layouts` is the one bundled plugin with a
              # standalone-CLI hat (`apply <layout>`, #1019) and so the one
              # with a `completions <shell>` subcommand to build (#1116); every
              # other bundled plugin is driven entirely by the wire protocol
              # and has no argv to complete.
              // pkgs.lib.optionalAttrs (name == "hytte-plugin-niri-layouts") {
                hasCompletions = true;
              }
            )
          );

          # The `hytte-infobroker` CLI package (#562), mirroring the `packages`
          # output above. Wired into `checks` below for the same #449 reason as
          # `bundledPlugins`: without it, `nix flake check` could stay green
          # while `nix build .#hytte-infobroker` was actually broken.
          hytte-infobroker = pkgs.callPackage ./nix/plugin.nix {
            inherit workspace;
            name = "hytte-infobroker";
            description = "trollshell consent-gated agent-bridge broker CLI (#487)";
            # The CLI's own `completions <shell>` subcommand (#1116).
            hasCompletions = true;
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
            description = "trollshell keyless same-uid-socket OpenAI-compatible bridge to headless Claude Code (#584, #993)";
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
          # this is a slice — a `cp` + a GApps wrap, no cargo and no crane —
          # exactly like the plugin packages above; since #1257 the binary it
          # copies is `probes`' (the checks-universe compile), not
          # `workspace`'s. Before #588 it was its own `buildDepsOnly` +
          # `buildPackage` pair with its own `src` filter, i.e. a second full
          # dependency compile per cold flake check.
          probe = pkgs.callPackage ./nix/probe.nix { inherit workspace probes; };
          # The hytte-services `wifi_probe` example binary, for the
          # wifi-nm-nixos-test below. Same slice treatment (#588/#1257) — it
          # was the third full dependency compile.
          wifiProbe = pkgs.callPackage ./nix/wifi-probe.nix { inherit workspace probes; };
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
            trollshell-agent-window
            hytte-infobroker
            hytte-claude-bridge
            ;

          # The shell's runtime closure must carry no WebKitGTK 6.0 (#1130 M1).
          # A `runCommand` over `exportReferencesGraph` — no compile, so it is
          # red in seconds, and it reads nix's own answer about the closure
          # rather than re-deriving one. See nix/checks/ for the whole story,
          # including why it names the ABI.
          shell-has-no-web-engine = pkgs.callPackage ./nix/checks/shell-has-no-web-engine.nix {
            inherit trollshell trollshell-control-center;
          };

          # The two nixosTest probe examples (`probe`, `wifi_probe`) must
          # never creep back into the `workspace` compile every package
          # output slices from (#1257) — see nix/checks/workspace-ships-no-
          # probes.nix for the full mechanism and how to falsify it.
          workspace-ships-no-probes = pkgs.callPackage ./nix/checks/workspace-ships-no-probes.nix {
            inherit workspace;
          };

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

          # The static counterpart to `clippy`, for a defect clippy cannot see
          # (#831): a `bind*` call site that discards the closure's own widget
          # parameter and uses a captured strong clone of the same widget
          # instead, pinning it for the binding's lifetime and defeating the
          # `WeakRef` contract in crates/hytte-reactive/src/bind.rs:16-22
          # (#224).
          #
          # This exists because the defect has already recurred once by the
          # only other means available: #772 fixed four sites and closed on a
          # hand-read inventory of four; #831 re-derived the list and found
          # twelve, so two thirds of it had been missed. #834 fixed all twelve
          # with the scanner this check now runs. A unit test can only cover one
          # site at a time and only where the widget is constructible without a
          # registered `Registry` — ten of the twelve are not — whereas the bug
          # is purely *syntactic*, which is exactly what a scanner sees.
          #
          # Deliberately not a crane derivation: it needs no compile, no
          # cargoArtifacts and no target dir, so it goes red in seconds on a
          # cold checkout instead of behind a full workspace build.
          #
          # `cd ${self}` rather than staging the three scanned trees into a
          # sandbox: it keeps the scanned-root list in one place (the script's
          # `DEFAULT_ROOTS`), makes every reported `file:line` a path a
          # developer can open as-is, and costs nothing extra — `formatting`
          # already realises `self` on every flake check. A root that goes
          # missing (a renamed tree) exits 2 rather than passing vacuously, as
          # does a scan that sees implausibly few call sites.
          bind-pins = pkgs.runCommand "trollshell-bind-pin-check" { nativeBuildInputs = [ pkgs.python3 ]; } ''
            cd ${self}
            python3 nix/lint-bind-pins.py
            touch $out
          '';

          # The root `[workspace.lints]` table and the two `unsafe` islands'
          # hand-mirrored copies (`crates/hytte-ecal/Cargo.toml`,
          # `crates/hytte-gl/Cargo.toml`) must agree on every lint except the
          # one `unsafe_code` line — Cargo's workspace-lints inheritance is
          # all-or-nothing, which is why the copies exist at all. Nothing
          # enforced that: deleting `pedantic` from an island leaves `cargo
          # clippy --workspace --all-targets -- -D warnings` and the whole of
          # `nix flake check` green, and one of the two crates where `unsafe`
          # is legal quietly stops being pedantic-checked (#1179, from the
          # #1162 sweep). Same posture as `bind-pins` above: a source-level
          # defect no compile in this flake can see, so a script rather than a
          # test, with no cargoArtifacts so it goes red in seconds. It also
          # asserts the other direction — every *non*-island member inherits
          # with `[lints] workspace = true` — so a third island cannot appear
          # unnoticed. `nix/lint-lints-tables.py`'s own header has the full
          # story, including why the five documented FFI-only `allow`s in
          # `hytte-ecal` are declared in the script rather than waved through.
          lints-tables =
            pkgs.runCommand "trollshell-lints-tables-check" { nativeBuildInputs = [ pkgs.python3 ]; }
              ''
                cd ${self}
                python3 nix/lint-lints-tables.py
                touch $out
              '';

          # `programs.trollshell.config.core-leds`'s nix-side vocabulary
          # (`style`/`fill` enums, `rows`'s cap) hand-mirrors Rust's
          # `DisplayStyle`/`parse_core_leds_fill`/`MAX_ROWS` (#1041, #1081
          # review M6) with nothing failing if the two drift — and this
          # option is explicitly the template nine more subsystem families
          # copy. Same posture as `bind-pins` above: a source-level defect no
          # compile in this flake can see, so a script rather than a test,
          # with no cargoArtifacts so it goes red in seconds.
          #
          # Deliberately NOT a `cargo test`: a first version of this lived in
          # `trollshell/src/config/core_leds.rs` reading
          # `nix/module-common.nix` off disk via `CARGO_MANIFEST_DIR`, and it
          # passed locally while failing in CI — `nix/package.nix`'s crane
          # source filter keeps only `.rs`/`.toml`/lockfile/CSS/shader files,
          # so the sandboxed `workspace` compile `cargo test --workspace`
          # runs inside has no `.nix` files at all (the same `include_str!`
          # trap CLAUDE.md documents for `assets/`, reached by
          # `std::fs::read_to_string` instead). Widening the crane filter to
          # keep `*.nix` was rejected — every `.nix` edit would then
          # invalidate `workspace`'s source hash and force a full recompile.
          # `nix/lint-config-vocab.py`'s own header has the full story —
          # including why it is `lint-config-vocab.py` and `config-vocab`
          # rather than the `core-leds` spelling both carried until #1237:
          # `agents` (#1227 item 1) is the second
          # `programs.trollshell.config.<subsystem>` family to hand-mirror a
          # Rust vocabulary here, of nine expected, and the check now covers
          # its `poll_seconds` bounds and the *set of keys itself* on both
          # sides (a Rust field with no nix option leaf is drift the byte
          # fixture cannot see).
          config-vocab =
            pkgs.runCommand "trollshell-config-vocab-check" { nativeBuildInputs = [ pkgs.python3 ]; }
              ''
                cd ${self}
                python3 nix/lint-config-vocab.py
                touch $out
              '';

          # `programs.trollshell.claudeBridge.baseUrl` (`nix/module-common.nix`)
          # is a plain nix string literal naming the same socket
          # `crates/hytte-ai-providers/src/unix.rs`'s `BRIDGE_SOCKET_DIR`/
          # `BRIDGE_SOCKET_FILE`/`BRIDGE_BASE_URL` constants define, and both
          # that crate's and `crates/hytte-claude-bridge/src/main.rs`'s module
          # docs restate in prose (#1099 review M1, #1100; hardened against a
          # #1103 adversarial review's M1/M2/L1/L2 on the nix-to-Rust seam and
          # the doc-prose scan). The Rust constants are already pinned against
          # raw literals by plain `cargo test`
          # (`bridge_url_resolves_to_the_bridge_socket_path`,
          # `the_socket_path_is_the_one_the_client_dials`); nothing compiles
          # the nix literal or either crate's doc prose against them — #1099's
          # review measured that a rename of the Rust file name left
          # `cargo test`, `cargo clippy` and every module-eval check green,
          # because the daemon binds a path only a live session would notice
          # went stale. Same posture as `bind-pins` above: a source-level
          # defect no compile in this flake can see, so a script rather than a
          # test, with no cargoArtifacts so it goes red in seconds.
          # `nix/lint-bridge-socket.py`'s own header has the full story,
          # including why this is not a `cargo test` (the `config-vocab`
          # crane-filter reasoning applies unchanged).
          bridge-socket =
            pkgs.runCommand "trollshell-bridge-socket-check" { nativeBuildInputs = [ pkgs.python3 ]; }
              ''
                cd ${self}
                python3 nix/lint-bridge-socket.py
                touch $out
              '';

          # The preem GL renderer's shaders, compiled in the dialect the shell
          # compiles them in (#893 stage B). Same posture and same reasons as
          # `bind-pins` above: a source-level defect no compile in this flake
          # can see, checked by a script rather than a test, with no
          # cargoArtifacts so it goes red in seconds.
          #
          # Nothing else looks inside those files. They are `include_str!`'d
          # `&'static str`s until a *driver* compiles them, and no check here
          # does — `system-tests` gained a software GL driver (llvmpipe, via
          # `mesa`, #1036) so its own GL-context tests now run for real, but
          # those tests exercise only inline dummy shaders defined in the test
          # module, never these actual `preem_gl`/shader-widget source files.
          # So without this a typo'd identifier ships green and surfaces as a
          # blank chip on glass.
          #
          # The design spec's CI table named **naga** for this row. Measured, it
          # cannot do the job: naga 26's GLSL frontend rejects the entire ES
          # profile — `#version 300/310/320 es` each come back
          # `InvalidVersion(N)` + `InvalidProfile("es")`, so the spec's own
          # fallback (decision 6, "drop to 310 es") does not reach either — and
          # at `#version 450` it stops on `NotImplemented("variable
          # qualifier")` for the `flat in` / `precision` declarations these
          # shaders are written with. The spec preferred naga because glslang is
          # C++ FFI that would have to live in the `hytte-gl` unsafe island: a
          # real objection to *linking* a validator in for #893's untrusted
          # plugin shaders, and none at all to running the binary at build time.
          # So this is glslang, it validates the real `#version 320 es`, and it
          # adds zero `Cargo.lock` entries.
          glsl =
            pkgs.runCommand "trollshell-glsl-check"
              {
                nativeBuildInputs = [
                  pkgs.python3
                  pkgs.glslang
                ];
              }
              ''
                cd ${self}
                python3 nix/lint-glsl.py
                touch $out
              '';

          # Run the hermetic internals suite (#1115): `cargo test --workspace`
          # WITHOUT `--features system-tests` — the same command
          # `nix/package.nix`'s `workspace` derivation ran under its own
          # `doCheck = true` before #1115 turned that off so a consumer's
          # `nix build` doesn't pay for it (and so the deps stage feeding it,
          # `cargoArtifactsBinOnly`, doesn't have to compile the
          # dev-dependency graph for every consumer either — see that file's
          # `cargoArtifacts`/`cargoArtifactsBinOnly` split). Same tests, same
          # gate, only moved to where the gate lives: `nix flake check`
          # already builds the package (#449) and already runs
          # `system-tests` as its own check below; the hermetic suite had no
          # check of its own and rode the package instead.
          #
          # Shape matches hyperhive's own split for the identical problem
          # (Mara, #1115): `hyperhive/nix/rust.nix:43-94` two named
          # `buildDepsOnly` caches, one per audience;
          # `hyperhive/nix/packages/default.nix:61-68` the deploy path takes
          # the no-test-graph one; `hyperhive/nix/checks.nix:58-73` the check
          # takes the one WITH the test graph via a plain `craneLib.cargoTest`
          # rather than `mkCargoDerivation`. Same trade here: `cargoTest`
          # hardcodes `checkPhaseCargoCommand` to `cargoWithProfile test
          # ${cargoExtraArgs} ${cargoTestExtraArgs}`, which is fine for a
          # plain hermetic run — `system-tests` below needs `mkCargoDerivation`
          # directly only because it wraps that command in `xvfb-run` for a
          # display, which this check doesn't need.
          #
          # `trollshell.passthru.commonArgs` already carries `cargoExtraArgs =
          # "--workspace --locked"` (#572, one cargo scope for every stage),
          # so — unlike hyperhive's own `cargo-test`, which has no such shared
          # scope and spells `cargoTestExtraArgs = "--workspace";` itself —
          # nothing further is needed here to get `--workspace`; leaving
          # `cargoTestExtraArgs` at `cargoTest`'s own default (`""`) matches
          # how `clippy` above already reuses the same `cargoExtraArgs` and
          # only adds its own check-specific flags.
          #
          # `cargoArtifacts = trollshell.passthru.cargoArtifacts`, NOT
          # `cargoArtifactsBinOnly`: since #1115 the former is the one built
          # with `buildDepsOnly`'s own default `doCheck = true` (dev-deps +
          # test harnesses compiled and cached), same cache `clippy` above
          # already reuses — the latter has no dev-dependency graph for a
          # `cargo test` run to build against.
          workspace-tests = craneLib.cargoTest (
            trollshell.passthru.commonArgs
            // {
              cargoArtifacts = trollshell.passthru.cargoArtifacts;
              # The same icon-theme gate `nix/package.nix`'s `preCheck` carried
              # before #1115 — see
              # `every_icon_name_exists_in_the_adwaita_theme_on_the_search_path`
              # (crates/hytte-plugin-niri-layouts/src/plugin.rs). nixpkgs puts
              # no icon theme on a build's `XDG_DATA_DIRS` of its own accord,
              # so without this the test silently *skips* instead of gating
              # anything; `TROLLSHELL_REQUIRE_ICON_THEME=1` turns a missing
              # theme into a failure instead, so it can never rot back into
              # a silent no-op. `system-tests` below carries its own copy for
              # the same reason (#1053 review) and this check inherits
              # neither.
              preCheck = ''
                export XDG_DATA_DIRS="${pkgs.adwaita-icon-theme}/share''${XDG_DATA_DIRS:+:$XDG_DATA_DIRS}"
                export TROLLSHELL_REQUIRE_ICON_THEME=1
              '';
              # Leaf/terminal check: nothing consumes its target dir. Same
              # `doInstallCargoArtifacts = false` reasoning as `system-tests`
              # below — crane's default packs the whole (multi-GiB) target
              # dir into `$out`, which is pure waste for a check nothing
              # chains off of.
              doInstallCargoArtifacts = false;
            }
          );

          # Run the `system-tests` cargo-feature bucket (#232): the
          # whole-file-`#![cfg(feature = "system-tests")]` integration tests
          # in hytte-bus/hytte-reactive/hytte-ui, plus the `#[cfg(all(test,
          # feature = "system-tests"))]` GTK unit-test modules in hytte-ui,
          # PLUS the #893 stage B CPU/GL parity harness. Split out to
          # nix/checks/system-tests.nix (#1102), mirroring how `packages`
          # already lives under `nix/*.nix` — see that file for the full
          # rationale (why mkCargoDerivation, the llvmpipe/systemd preCheck,
          # the parity gate). This call passes exactly what the block used
          # to close over here: `craneLib` and the two
          # `trollshell.passthru.*` values it read off the package
          # derivation (`pkgs` is auto-supplied by `callPackage`).
          system-tests = pkgs.callPackage ./nix/checks/system-tests.nix {
            inherit craneLib;
            commonArgs = trollshell.passthru.commonArgs;
            cargoArtifacts = trollshell.passthru.cargoArtifacts;
          };

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
                            # The #993 shape: a same-uid socket, not a port.
                            # Spelled out rather than referenced through
                            # `config` (this fixture module takes no arguments)
                            # — the probe below asserts it equals the read-only
                            # `claudeBridge.baseUrl` a real config would use.
                            PET_LLM_URL = "unix://$XDG_RUNTIME_DIR/trollshell/claude-bridge.sock";
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
                # #813 item 2: the fixture above sets systemd.target =
                # "niri-session.target", so the rendered launch state must carry
                # that same value as its top-level "target" key (#707) — the
                # plugin units and the shell's own unit have to bind to the
                # same session target, or teardown comes apart again. The
                # complementary "absent when left at the default" half of this
                # invariant needs its own fixture (this one already forces a
                # non-default target for the LLM-unit assertions below) — see
                # hm-module-plugin-target-default.
                assert pluginsState.target == "niri-session.target";
                # The claude bridge (#866) is a PLUGIN now, not a unit: enabling
                # it must render `plugins.claude-bridge` into the launch state
                # and declare no `trollshell-claude-bridge` unit at all. The
                # negative is the load-bearing half — a stray unit alongside the
                # plugin entry would run the bridge twice and the second copy
                # would find the socket already live and refuse to start.
                assert !(units ? trollshell-claude-bridge);
                assert
                  let
                    b = pluginsState.plugins.claude-bridge;
                  in
                  b.exec == pkgs.lib.getExe stubClaudeBridge
                  && b.enabled
                  # #993: no port is rendered at all, because the bridge
                  # listens on a same-uid socket whose path is not
                  # configurable. The negative is the load-bearing half — a
                  # CLAUDE_BRIDGE_PORT back in this env would mean somebody
                  # restored a listener every local uid can reach.
                  && !(b.env ? CLAUDE_BRIDGE_PORT)
                  && b.env.CLAUDE_BRIDGE_MODEL == "claude-haiku-4-5"
                  && b.env.CLAUDE_BRIDGE_TIMEOUT_SECS == "15"
                  # The default mode, i.e. one that spawns `claude`…
                  && b.env.CLAUDE_BRIDGE_MODE == "subscription"
                  # …so the `anthropic` slot must NOT be declared: an injected
                  # ANTHROPIC_API_KEY is a startup refusal there, not a
                  # credential (#752). The `api` half of this invariant has its
                  # own fixture, hm-module-claude-bridge-api below.
                  && b.secrets == [ ]
                  # THE BILLING/REDIRECT SCRUB (#866, extended #994). The
                  # retired unit's `UnsetEnvironment=` is re-expressed as EMPTY
                  # env values, which is what stops an inherited
                  # ANTHROPIC_API_KEY — or a stray ANTHROPIC_BASE_URL — from
                  # restart-looping the bridge, or silently redirecting it, in
                  # the default mode. Losing any of these is a silent
                  # regression that only bites the users who happen to export
                  # one, so each is asserted by content.
                  && b.env.ANTHROPIC_API_KEY == ""
                  && b.env.ANTHROPIC_AUTH_TOKEN == ""
                  && b.env.CLAUDE_CODE_OAUTH_TOKEN == ""
                  && b.env.CLAUDE_CODE_USE_BEDROCK == ""
                  && b.env.CLAUDE_CODE_USE_VERTEX == ""
                  && b.env.CLAUDE_CODE_USE_FOUNDRY == ""
                  && b.env.ANTHROPIC_BASE_URL == ""
                  && b.env.ANTHROPIC_BEDROCK_BASE_URL == ""
                  && b.env.ANTHROPIC_VERTEX_BASE_URL == ""
                  && b.env.ANTHROPIC_FOUNDRY_BASE_URL == ""
                  && b.env.ANTHROPIC_FOUNDRY_RESOURCE == ""
                  && b.env.ANTHROPIC_FOUNDRY_AUTH_TOKEN == ""
                  && b.env.ANTHROPIC_FOUNDRY_API_KEY == ""
                  && b.env.ANTHROPIC_CUSTOM_HEADERS == "";
                # #993, the client half: `claudeBridge.baseUrl` is read-only and
                # is exactly what a plugin's `*_LLM_URL` has to carry — the
                # replacement for the two-things-must-agree-on-one-number job
                # `claudeBridge.port` used to do. `$XDG_RUNTIME_DIR` is left
                # unexpanded on purpose; logind mints it at login, so nix
                # cannot know it and `hytte-ai-providers` resolves it in the
                # consuming plugin's own process.
                assert pluginsState.plugins.pet.env.PET_LLM_URL == cfg.programs.trollshell.claudeBridge.baseUrl;
                assert pkgs.lib.hasPrefix "unix://" cfg.programs.trollshell.claudeBridge.baseUrl;
                assert units ? trollshell-pet-brain;
                # `builtins.toString` because home-manager's unitOption merge
                # hands some of these back list-wrapped (ExecStart below is
                # `[ "…" ]`, not `"…"`) — toString is identity on a plain string
                # and space-joins the one-element list, so the assertions hold
                # whichever shape the merge produces.
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

          # #813 item 2's other half: a *separate* homeManagerConfiguration
          # evaluation, deliberately not folded into the hm-module fixture
          # above. That fixture always sets systemd.target =
          # "niri-session.target" (needed to exercise the LLM-backend-unit
          # assertions), so it can only prove the "target" key is emitted
          # when set — never that it's OMITTED when left at the default. The
          # omission is the half that actually protects the #707 fingerprint
          # invariant: a default-configured session's rendered plugins.json
          # must stay byte-identical to the pre-#707 file, so upgrading
          # recycles no already-running plugin (see the pluginsState comment
          # in nix/hm-module.nix). It is also the half a later refactor
          # breaks silently — the plugins would keep launching, just bound to
          # the wrong target, which is the original #707 defect returning by
          # the back door. This check exists solely to give that invariant a
          # fixture, the same idiom nixos-module-nightlight below uses for a
          # different conditional.
          hm-module-plugin-target-default =
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
                      enableNixpkgsReleaseCheck = false;
                    };
                    programs.trollshell = {
                      enable = true;
                      package = stubPackage;
                      # systemd.target deliberately left unset (defaults to
                      # graphical-session.target) — the one thing this fixture
                      # exists to cover.
                      plugins.demo.package = stubPlugin;
                    };
                  }
                ];
              };
              cfg = hm.config;
              pluginsState = builtins.fromJSON (
                builtins.unsafeDiscardStringContext cfg.xdg.configFile."trollshell/plugins.json".text
              );
              probe =
                assert !(pluginsState ? target);
                builtins.deepSeq { inherit pluginsState; } "ok";
            in
            pkgs.runCommand "trollshell-hm-module-plugin-target-default-check" { inherit probe; } ''
              echo "$probe" >/dev/null
              touch $out
            '';

          # #866's mode-conditional secret slot, which the hm-module fixture
          # above can only ever prove one side of (it runs the default
          # `subscription` mode, where the slot must be ABSENT). This is the
          # other side: with `claudeBridge.mode = "api"` the rendered plugin
          # entry must declare the `anthropic` slot, so the launcher injects the
          # keyring's key as ANTHROPIC_API_KEY at spawn — the whole point of
          # moving the bridge onto the launcher, and the one path by which an
          # `api`-mode bridge gets a key without a file.
          #
          # The pair is genuinely two-sided and neither half is decorative:
          # declaring the slot in a `claude` mode stops the bridge starting at
          # all (envguard's #752 refusal), and not declaring it in `api` mode
          # leaves the key file as the only source. A refactor that dropped the
          # `cb.mode == "api"` guard would keep this check green and break the
          # other; dropping the slot entirely would do the reverse.
          hm-module-claude-bridge-api =
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
                      enableNixpkgsReleaseCheck = false;
                    };
                    programs.trollshell = {
                      enable = true;
                      package = stubPackage;
                      claudeBridge = {
                        enable = true;
                        package = stubClaudeBridge;
                        mode = "api";
                      };
                    };
                  }
                ];
              };
              cfg = hm.config;
              pluginsState = builtins.fromJSON (
                builtins.unsafeDiscardStringContext cfg.xdg.configFile."trollshell/plugins.json".text
              );
              probe =
                assert !(cfg.systemd.user.services ? trollshell-claude-bridge);
                assert
                  let
                    b = pluginsState.plugins.claude-bridge;
                  in
                  b.env.CLAUDE_BRIDGE_MODE == "api"
                  && b.secrets == [ "anthropic" ]
                  # The scrub is unconditional — the same four empty values in
                  # api mode. That is not a contradiction with the slot above:
                  # the launcher appends injected secrets AFTER the declared env
                  # and systemd lets the later assignment win, so the empty value
                  # is the floor a keyring key overrides (and the key file's
                  # fallback when there is none).
                  && b.env.ANTHROPIC_API_KEY == "";
                builtins.deepSeq { inherit pluginsState; } "ok";
            in
            pkgs.runCommand "trollshell-hm-module-claude-bridge-api-check" { inherit probe; } ''
              echo "$probe" >/dev/null
              touch $out
            '';

          # #1161: `programs.trollshell.plugins.<id>.mount` must be strictly
          # additive — a plugin that never sets it renders the exact same
          # `plugins.json` entry as before this option existed. Two plugins,
          # one covering each half: `right` sets `mount` and must carry
          # `HYTTE_PLUGIN_MOUNT` in its rendered `env`; `demo` doesn't, and
          # its ENTIRE rendered entry is asserted against `expectedDemo` —
          # every field `nix/hm-module.nix`'s `pluginsState` puts on a
          # plugin, spelled out here rather than sourced from a captured
          # historical eval (a rename or a stray extra key fails this the
          # same way an accidental `HYTTE_PLUGIN_MOUNT` leak would). Named
          # `demo` rather than `left` (its name before #1284) so its
          # attribute agrees with `stubPlugin`'s own inferred manifest id
          # ("demo", from `writeShellScriptBin "hytte-plugin-demo"`) and
          # stays free of a `HYTTE_PLUGIN_ID` override too — this check is
          # about `mount` alone; `hm-module-plugin-id` below covers the
          # other knob with the same fixture shape. Nix attrset equality
          # after `fromJSON` is used rather than a raw `diff` — unlike
          # `hm-module-agents-fixture` above, there is no TOML formatting to
          # pin here, and comparing parsed values catches a real regression
          # the way a byte diff would while staying insensitive to JSON key
          # order (`builtins.toJSON` already sorts keys, so the two coincide
          # in practice, but the parsed comparison is the one that says what
          # it means).
          hm-module-plugin-mount =
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
                      enableNixpkgsReleaseCheck = false;
                    };
                    programs.trollshell = {
                      enable = true;
                      package = stubPackage;
                      plugins = {
                        demo.package = stubPlugin;
                        right = {
                          package = stubPlugin;
                          mount = "SidebarRightTop";
                        };
                        # #1260 review F5: `mount` and `env.HYTTE_PLUGIN_MOUNT`
                        # are one knob and `mount` wins, which
                        # `nix/module-common.nix` now asserts on rather than
                        # discarding the hand-set value silently. This is the
                        # *agreeing* half — redundant, not a conflict — and it
                        # must still evaluate, with every predicate true. The
                        # disagreeing half is `hm-module-plugin-mount-conflict`
                        # below, which pins that the assertion actually fires.
                        agreeing = {
                          package = stubPlugin;
                          mount = "BarRight";
                          env.HYTTE_PLUGIN_MOUNT = "BarRight";
                        };
                      };
                    };
                  }
                ];
              };
              cfg = hm.config;
              pluginsState = builtins.fromJSON (
                builtins.unsafeDiscardStringContext cfg.xdg.configFile."trollshell/plugins.json".text
              );
              expectedDemo = {
                exec = pkgs.lib.getExe stubPlugin;
                env = { };
                secrets = [ ];
                enabled = true;
              };
              assertionPredicates = map (a: a.assertion) cfg.assertions;
              probe =
                assert pluginsState.plugins.right.env.HYTTE_PLUGIN_MOUNT == "SidebarRightTop";
                assert pluginsState.plugins.demo == expectedDemo;
                assert pluginsState.plugins.agreeing.env.HYTTE_PLUGIN_MOUNT == "BarRight";
                assert builtins.all (p: p) assertionPredicates;
                builtins.deepSeq { inherit pluginsState assertionPredicates; } "ok";
            in
            pkgs.runCommand "trollshell-hm-module-plugin-mount-check" { inherit probe; } ''
              echo "$probe" >/dev/null
              touch $out
            '';

          # #1284: `programs.trollshell.plugins.<id>` renders `HYTTE_PLUGIN_ID`
          # from the attribute name whenever it disagrees with the package's
          # own manifest id — the same shape `hm-module-plugin-mount` above
          # pins for `mount` → `HYTTE_PLUGIN_MOUNT` (#1161), on the #1260
          # review F5 precedent. `demo` (attribute name == manifest id, both
          # "demo" — `stubPlugin` is `writeShellScriptBin "hytte-plugin-demo"`,
          # so stripping the `hytte-plugin-` prefix off its binary name gives
          # "demo") renders nothing: its ENTIRE rendered entry is asserted
          # against `expectedDemo`, spelling out every field
          # `nix/hm-module.nix`'s `pluginsState` puts on a plugin, so a stray
          # `HYTTE_PLUGIN_ID` leak fails this the same way a missing field
          # would. `demo-side` (attribute name != manifest id) carries
          # `HYTTE_PLUGIN_ID = "demo-side"`. `agreeing` sets
          # `env.HYTTE_PLUGIN_ID` by hand to the SAME value the override would
          # render — the redundant, not-a-conflict half of #1284's precedence
          # assertion in `nix/module-common.nix`, which must keep evaluating
          # with every predicate true. The disagreeing half is
          # `nixos-module-plugin-id-conflict` below (home-manager evaluates
          # `config.assertions` eagerly and throws rather than handing back an
          # inspectable false predicate — the same reason
          # `nixos-module-plugin-mount-conflict` is NixOS-side only); the
          # disagreeing-AND-attribute-name-already-agrees-with-the-manifest-id
          # hole that opened is `hm-module-plugin-id-conflict-unconditional`
          # just below (#1284 fix round, review MED 1).
          #
          # `claude-bridge` (over `stubClaudeBridge`, `writeShellScriptBin
          # "hytte-claude-bridge"`) pins the OTHER prefix shape — the bare
          # `hytte-` fallback `inferManifestId` falls through to for the
          # standalone-hat binaries — against the REAL deployment shape
          # `nix/hm-module.nix:847`'s own `claudeBridge.*` rendering uses
          # (attr `claude-bridge` over `pkgs.hytte-claude-bridge`): its entire
          # rendered entry must equal `expectedClaudeBridge`, `env = { }`
          # included, the same full-equality shape `demo` gets rather than a
          # single-field assertion an extra `HYTTE_PLUGIN_ID` key could slide
          # past (#1284 fix round, review MED 2 — the `left` → `demo` rename
          # in `hm-module-plugin-mount` below had deleted the only fixture
          # that incidentally caught this). The two `inferManifestId` asserts
          # tie this check to the SHARED heuristic directly
          # (`hm._module.args.inferManifestId`, off `nix/module-common.nix` —
          # #1284 fix round, review LOW 3), not just to its rendered effect.
          hm-module-plugin-id =
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
                      enableNixpkgsReleaseCheck = false;
                    };
                    programs.trollshell = {
                      enable = true;
                      package = stubPackage;
                      plugins = {
                        demo.package = stubPlugin;
                        demo-side.package = stubPlugin;
                        agreeing = {
                          package = stubPlugin;
                          env.HYTTE_PLUGIN_ID = "agreeing";
                        };
                        claude-bridge.package = stubClaudeBridge;
                      };
                    };
                  }
                ];
              };
              cfg = hm.config;
              pluginsState = builtins.fromJSON (
                builtins.unsafeDiscardStringContext cfg.xdg.configFile."trollshell/plugins.json".text
              );
              expectedDemo = {
                exec = pkgs.lib.getExe stubPlugin;
                env = { };
                secrets = [ ];
                enabled = true;
              };
              expectedClaudeBridge = {
                exec = pkgs.lib.getExe stubClaudeBridge;
                env = { };
                secrets = [ ];
                enabled = true;
              };
              assertionPredicates = map (a: a.assertion) cfg.assertions;
              probe =
                # `_module.args` is exposed as a sibling of `config` on the
                # `evalModules` result (`hm`), not folded into `config`
                # itself (`lib/modules.nix` explicitly strips it back out of
                # `config` and re-attaches it at the top level) — hence `hm.`
                # here and not `cfg.`.
                assert hm._module.args.inferManifestId stubPlugin == "demo";
                assert hm._module.args.inferManifestId stubClaudeBridge == "claude-bridge";
                assert pluginsState.plugins.demo == expectedDemo;
                assert pluginsState.plugins.demo-side.env.HYTTE_PLUGIN_ID == "demo-side";
                assert pluginsState.plugins.agreeing.env.HYTTE_PLUGIN_ID == "agreeing";
                assert pluginsState.plugins.claude-bridge == expectedClaudeBridge;
                assert builtins.all (p: p) assertionPredicates;
                builtins.deepSeq { inherit pluginsState assertionPredicates; } "ok";
            in
            pkgs.runCommand "trollshell-hm-module-plugin-id-check" { inherit probe; } ''
              echo "$probe" >/dev/null
              touch $out
            '';

          # #1284 fix round, review MED 1's regression pin: the conflict
          # guard above used to be SKIPPED exactly where the attribute name
          # already agrees with the inferred manifest id — `demo` (attribute
          # name == manifest id, so nothing renders) with an explicit,
          # disagreeing `env.HYTTE_PLUGIN_ID` evaluated clean on both
          # platforms, silently shipping a launch whose systemd unit name
          # and `Register` id disagreed (exactly what this option exists to
          # remove; `nix/module-common.nix` states the invariant
          # unconditionally: "The attribute name IS the plugin's launch-time
          # id"). The guard is now UNCONDITIONAL — see that file's own
          # comment on the assertion — so this must now fail to evaluate.
          # The fixture is `hm-module-plugin-id`'s own `demo` entry (package
          # `stubPlugin`, inferred id "demo") with
          # `env.HYTTE_PLUGIN_ID = "something-else"` instead of nothing.
          # home-manager evaluates `config.assertions` eagerly while
          # building `hm.config` and throws rather than handing back an
          # inspectable false predicate (the same reason
          # `nixos-module-plugin-mount-conflict` is NixOS-side only), so this
          # can only report success/failure, via `tryEval`. #1081 review
          # M4's control-arm shape: `tryEval` alone can't distinguish "the
          # conflict assertion rejected it" from "some unrelated attribute
          # stopped existing", so the control below — the identical fixture
          # with no `env.HYTTE_PLUGIN_ID` at all — has to keep succeeding.
          # The NixOS twin, which CAN inspect the false predicate directly,
          # is `nixos-module-plugin-id-conflict-unconditional` below.
          hm-module-plugin-id-conflict-unconditional =
            let
              fixture =
                pluginId:
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
                          enableNixpkgsReleaseCheck = false;
                        };
                        programs.trollshell = {
                          enable = true;
                          package = stubPackage;
                          plugins.demo = {
                            package = stubPlugin;
                          }
                          // pkgs.lib.optionalAttrs (pluginId != null) { env.HYTTE_PLUGIN_ID = pluginId; };
                        };
                      }
                    ];
                  };
                in
                builtins.unsafeDiscardStringContext hm.config.xdg.configFile."trollshell/plugins.json".text;
              result = builtins.tryEval (builtins.deepSeq (fixture "something-else") "ok");
              control = builtins.tryEval (builtins.deepSeq (fixture null) "ok");
              probe =
                assert !result.success;
                assert control.success;
                builtins.deepSeq { inherit result control; } "ok";
            in
            pkgs.runCommand "trollshell-hm-module-plugin-id-conflict-unconditional-check" { inherit probe; } ''
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

          # #1200: hytte-plugin-usage was deleted (#320's Claude usage-limits
          # monitor — a Grafana public-dashboard poll that never had a real
          # dashboard URL, so it only ever rendered its own empty state). The
          # `plugins` option is an `attrsOf` submodule keyed by an arbitrary
          # id string, so "usage" was never a declared option path and
          # `lib.mkRemovedOptionModule` has nothing to attach to —
          # `nix/module-common.nix` asserts on the merged `plugins` set
          # instead (unconditionally, so both platforms' default-config
          # fixtures above see it too, staying part of the "every predicate
          # true" guarantee those checks make). This fixture is the same
          # shape as `nixos-module-nightlight` above: set the one thing that
          # should trip exactly one predicate, and check it's the right one
          # by message content, not by count.
          nixos-module-plugin-removed-1200 =
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
                      # The one thing this fixture exists to set: a config
                      # that still names the removed `usage` plugin id must
                      # trip nix/module-common.nix's assertion rather than
                      # silently rendering a launch-state entry for a
                      # `package` nobody built.
                      plugins.usage.package = stubPlugin;
                    };
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
              falsePredicates = builtins.filter (a: !a.assertion) cfg.assertions;
              probe =
                assert builtins.length falsePredicates == 1;
                assert pkgs.lib.hasInfix "plugins.usage" (builtins.head falsePredicates).message;
                assert pkgs.lib.hasInfix "1200" (builtins.head falsePredicates).message;
                builtins.deepSeq { inherit falsePredicates; } "ok";
            in
            pkgs.runCommand "trollshell-nixos-module-plugin-removed-1200-check" { inherit probe; } ''
              echo "$probe" >/dev/null
              touch $out
            '';

          # The NixOS twin of `hm-module-plugin-mount` above (#1161) — same
          # fixture shape, read back through `environment.etc` instead of
          # `xdg.configFile` (see `nixos-module` above for why). `mount` is
          # declared once, in the shared `plugins` submodule
          # (`nix/module-common.nix`), so setting it under THIS module has to
          # render too, or the option would silently do nothing on a
          # NixOS-only install — this is what would catch that. Named `demo`
          # rather than `left` (its name before #1284) for the same reason
          # as the home-manager twin: agreeing with `stubPlugin`'s own
          # inferred manifest id keeps this fixture free of an incidental
          # `HYTTE_PLUGIN_ID`, which is `nixos-module-plugin-id` below's job.
          nixos-module-plugin-mount =
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
                      plugins = {
                        demo.package = stubPlugin;
                        right = {
                          package = stubPlugin;
                          mount = "SidebarRightTop";
                        };
                        # See the home-manager twin: the agreeing half of
                        # #1260 review F5's precedence assertion, which must
                        # keep evaluating.
                        agreeing = {
                          package = stubPlugin;
                          mount = "BarRight";
                          env.HYTTE_PLUGIN_MOUNT = "BarRight";
                        };
                      };
                    };
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
              pluginsState = builtins.fromJSON (
                builtins.unsafeDiscardStringContext cfg.environment.etc."xdg/trollshell/plugins.json".text
              );
              expectedDemo = {
                exec = pkgs.lib.getExe stubPlugin;
                env = { };
                secrets = [ ];
                enabled = true;
              };
              # The `plugins.<id>.mount` precedence assertion lives in the
              # shared `nix/module-common.nix`, so it has to hold on this
              # platform too — and this is what proves the agreeing fixture
              # above does not trip it. Predicates only, never `.message`:
              # NixOS ships assertions whose message is lazy and only
              # well-defined when the assertion fails, so filtering or
              # deepSeq'ing by message content would trip an unrelated
              # internal one (see `nixos-module`'s own comment).
              assertionPredicates = map (a: a.assertion) cfg.assertions;
              probe =
                assert pluginsState.plugins.right.env.HYTTE_PLUGIN_MOUNT == "SidebarRightTop";
                assert pluginsState.plugins.demo == expectedDemo;
                assert pluginsState.plugins.agreeing.env.HYTTE_PLUGIN_MOUNT == "BarRight";
                assert builtins.all (p: p) assertionPredicates;
                builtins.deepSeq { inherit pluginsState assertionPredicates; } "ok";
            in
            pkgs.runCommand "trollshell-nixos-module-plugin-mount-check" { inherit probe; } ''
              echo "$probe" >/dev/null
              touch $out
            '';

          # The other half of #1260 review F5: `mount` and
          # `env.HYTTE_PLUGIN_MOUNT` set to **different** values must be
          # refused, not silently resolved — both platform modules render
          # `plugin.env // (optionalAttrs … { HYTTE_PLUGIN_MOUNT = …; })`, so
          # `mount` wins with no warning and the hand-written string is
          # discarded.
          #
          # Mirror image of `nixos-module-plugin-mount` above, and the same
          # `falsePredicates` idiom `nixos-module-nightlight` uses: exactly
          # one predicate must be false, and it must be *this* one (matched
          # by message content, which is narrower and far more stable than a
          # count of NixOS's own ~1400 predicates). The assertion itself is
          # declared once, in the shared `nix/module-common.nix`, so pinning
          # it here pins it for the home-manager module too — and it is
          # deliberately pinned on THIS platform because home-manager
          # evaluates `config.assertions` eagerly while building `hm.config`,
          # which throws rather than handing back an inspectable false
          # predicate (measured while writing this check).
          nixos-module-plugin-mount-conflict =
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
                      plugins.conflicting = {
                        package = stubPlugin;
                        mount = "SidebarRightTop";
                        env.HYTTE_PLUGIN_MOUNT = "BarLeft";
                      };
                    };
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
              falsePredicates = builtins.filter (a: !a.assertion) cfg.assertions;
              probe =
                assert builtins.length falsePredicates == 1;
                assert pkgs.lib.hasInfix "HYTTE_PLUGIN_MOUNT" (builtins.head falsePredicates).message;
                builtins.deepSeq { inherit falsePredicates; } "ok";
            in
            pkgs.runCommand "trollshell-nixos-module-plugin-mount-conflict-check" { inherit probe; } ''
              echo "$probe" >/dev/null
              touch $out
            '';

          # The NixOS twin of `hm-module-plugin-id` above (#1284) — same
          # fixture shape (including the `claude-bridge` bare-`hytte-`-prefix
          # pin and the `inferManifestId` ties — see that check's own comment
          # for both), read back through `environment.etc` instead of
          # `xdg.configFile` (see `nixos-module` above for why). `HYTTE_PLUGIN_ID`
          # has no typed option of its own to declare once in the shared
          # `nix/module-common.nix` — the override IS the attribute name — so
          # setting up a plugin under a second id has to render on THIS
          # platform too, or the behaviour would silently differ between the
          # two modules that both build `pluginsState` off the same shared
          # `plugins` submodule.
          nixos-module-plugin-id =
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
                      plugins = {
                        demo.package = stubPlugin;
                        demo-side.package = stubPlugin;
                        # See the home-manager twin: the agreeing half of
                        # #1284's precedence assertion, which must keep
                        # evaluating.
                        agreeing = {
                          package = stubPlugin;
                          env.HYTTE_PLUGIN_ID = "agreeing";
                        };
                        claude-bridge.package = stubClaudeBridge;
                      };
                    };
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
              pluginsState = builtins.fromJSON (
                builtins.unsafeDiscardStringContext cfg.environment.etc."xdg/trollshell/plugins.json".text
              );
              expectedDemo = {
                exec = pkgs.lib.getExe stubPlugin;
                env = { };
                secrets = [ ];
                enabled = true;
              };
              expectedClaudeBridge = {
                exec = pkgs.lib.getExe stubClaudeBridge;
                env = { };
                secrets = [ ];
                enabled = true;
              };
              # The `plugins.<id>` id precedence assertion lives in the shared
              # `nix/module-common.nix`, so it has to hold on this platform
              # too — and this is what proves the agreeing fixture above does
              # not trip it. Predicates only, never `.message`: see
              # `nixos-module-plugin-mount`'s own comment for why.
              assertionPredicates = map (a: a.assertion) cfg.assertions;
              probe =
                # See `hm-module-plugin-id`'s own comment: `_module.args` is
                # a sibling of `config` on the `evalModules` result
                # (`nixos`), never folded into `config` itself.
                assert nixos._module.args.inferManifestId stubPlugin == "demo";
                assert nixos._module.args.inferManifestId stubClaudeBridge == "claude-bridge";
                assert pluginsState.plugins.demo == expectedDemo;
                assert pluginsState.plugins.demo-side.env.HYTTE_PLUGIN_ID == "demo-side";
                assert pluginsState.plugins.agreeing.env.HYTTE_PLUGIN_ID == "agreeing";
                assert pluginsState.plugins.claude-bridge == expectedClaudeBridge;
                assert builtins.all (p: p) assertionPredicates;
                builtins.deepSeq { inherit pluginsState assertionPredicates; } "ok";
            in
            pkgs.runCommand "trollshell-nixos-module-plugin-id-check" { inherit probe; } ''
              echo "$probe" >/dev/null
              touch $out
            '';

          # The other half of #1284: an attribute name that disagrees with
          # the package's manifest id (so a `HYTTE_PLUGIN_ID` override is
          # active) AND an explicit `env.HYTTE_PLUGIN_ID` set to a
          # **different** value must be refused, not silently resolved — both
          # platform modules merge the attribute name's override over `env`
          # on the right of `//`, so it would win with no warning and the
          # hand-written string would be discarded.
          #
          # Mirror image of `nixos-module-plugin-id` above, and the same
          # `falsePredicates` idiom `nixos-module-plugin-mount-conflict` uses:
          # exactly one predicate must be false, and it must be *this* one
          # (matched by message content). The assertion itself is declared
          # once, in the shared `nix/module-common.nix`, so pinning it here
          # pins it for the home-manager module too — and it is deliberately
          # pinned on THIS platform because home-manager evaluates
          # `config.assertions` eagerly while building `hm.config`, which
          # throws rather than handing back an inspectable false predicate
          # (the same reason `nixos-module-plugin-mount-conflict` is
          # NixOS-side only).
          nixos-module-plugin-id-conflict =
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
                      plugins.conflicting = {
                        package = stubPlugin;
                        env.HYTTE_PLUGIN_ID = "something-else";
                      };
                    };
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
              falsePredicates = builtins.filter (a: !a.assertion) cfg.assertions;
              probe =
                assert builtins.length falsePredicates == 1;
                assert pkgs.lib.hasInfix "HYTTE_PLUGIN_ID" (builtins.head falsePredicates).message;
                builtins.deepSeq { inherit falsePredicates; } "ok";
            in
            pkgs.runCommand "trollshell-nixos-module-plugin-id-conflict-check" { inherit probe; } ''
              echo "$probe" >/dev/null
              touch $out
            '';

          # The NixOS twin of `hm-module-plugin-id-conflict-unconditional`
          # above (#1284 fix round, review MED 1) — same `falsePredicates`
          # idiom `nixos-module-plugin-id-conflict` above uses, but the
          # fixture is the HOLE that check does not cover: attribute name
          # "demo" already agrees with `stubPlugin`'s inferred manifest id
          # ("demo"), which is exactly the case the guard used to skip
          # entirely because nothing would have rendered an override. The
          # guard is now unconditional (`nix/module-common.nix`'s own
          # comment on the assertion), so an explicit, disagreeing
          # `env.HYTTE_PLUGIN_ID` here must still trip exactly one false
          # predicate naming `HYTTE_PLUGIN_ID`.
          nixos-module-plugin-id-conflict-unconditional =
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
                      plugins.demo = {
                        package = stubPlugin;
                        env.HYTTE_PLUGIN_ID = "something-else";
                      };
                    };
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
              falsePredicates = builtins.filter (a: !a.assertion) cfg.assertions;
              probe =
                assert builtins.length falsePredicates == 1;
                assert pkgs.lib.hasInfix "HYTTE_PLUGIN_ID" (builtins.head falsePredicates).message;
                builtins.deepSeq { inherit falsePredicates; } "ok";
            in
            pkgs.runCommand "trollshell-nixos-module-plugin-id-conflict-unconditional-check" { inherit probe; }
              ''
                echo "$probe" >/dev/null
                touch $out
              '';

          # #1041: `programs.trollshell.config.core-leds` renders a base-layer
          # `core-leds.toml` spliced onto the trollshell unit's own
          # `XDG_CONFIG_DIRS` (`nix/hm-module.nix`'s `configBase` +
          # `configDirsUnitEnvironment`) — NOT under
          # `xdg.configFile`/`$XDG_CONFIG_HOME`, which is the OVERLAY
          # `crates/hytte-config/src/xdg.rs` reserves for a hand edit. This
          # check forces the module eval, resolves the rendered store path out
          # of the trollshell **unit**'s own `Service.Environment` — not the
          # login-shell `home.sessionVariables` surface, which #1081 review
          # M1 found gave `nix build` a green check while the unit itself had
          # gone dark (mutation N1: drop `XDG_CONFIG_DIRS` from
          # `Service.Environment` alone, leaving `home.sessionVariables`
          # untouched — `#568`'s exact lesson, applied to this variable
          # instead of a `TROLLSHELL_*` one) — and TOML round-trips its bytes
          # against the values set. The positive half of the #1041
          # module-eval coverage; the negative half (an unknown key) is
          # nixos-module-core-leds-unknown-key below, since the submodule type
          # is declared once in module-common.nix and shared by both modules.
          # `hm-module-core-leds-composes-with-xdg-systemdirs` below covers
          # the *other* half of H1 — that this doesn't collide with a user's
          # own `xdg.systemDirs.config`.
          hm-module-core-leds =
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
                      enableNixpkgsReleaseCheck = false;
                    };
                    programs.trollshell = {
                      enable = true;
                      package = stubPackage;
                      config.core-leds = {
                        style = "lcd";
                        rows = "rect";
                      };
                    };
                  }
                ];
              };
              cfg = hm.config;
              # Each entry of `Service.Environment` is a NIX string carrying
              # its own literal `"…"` quoting (`nix/hm-module.nix`'s
              # `"\"${name}=${value}\""`, for systemd's own list-of-quoted-
              # strings syntax) — find the `XDG_CONFIG_DIRS=` one and strip
              # both the embedded quotes and the key.
              environment = cfg.systemd.user.services.trollshell.Service.Environment;
              xdgEntry = pkgs.lib.findFirst (e: pkgs.lib.hasPrefix "\"XDG_CONFIG_DIRS=" e) null environment;
              # `assert` lives on `xdgValue` itself, the first thing that would
              # otherwise try (and fail, less legibly) to coerce a `null`
              # `xdgEntry` to a string — not on a separate `probe` binding
              # nobody downstream forces. The build script below interpolates
              # `${renderedFile}` directly, which is derived from `xdgValue`,
              # so this assert sits on the actual path evaluation takes; a
              # `probe`-shaped wrapper that nothing references is dead (a
              # mutation dropping `configDirsUnitEnvironment` still reds, just
              # via Nix's own "cannot coerce null to a string" instead of this
              # message — measured in review).
              xdgValue =
                assert xdgEntry != null;
                pkgs.lib.removeSuffix "\"" (pkgs.lib.removePrefix "\"XDG_CONFIG_DIRS=" xdgEntry);
              # The leading entry is `configBase` (nix/hm-module.nix): a
              # store path is never a mid-list entry, so splitting on ":" and
              # taking the head is exactly what the shell's own
              # `xdg::config_dirs` parse does.
              base = builtins.head (pkgs.lib.splitString ":" xdgValue);
              renderedFile = "${base}/trollshell/core-leds.toml";
            in
            pkgs.runCommand "trollshell-hm-module-core-leds-check" { nativeBuildInputs = [ pkgs.python3 ]; } ''
              python3 -c '
              import tomllib
              with open("${renderedFile}", "rb") as f:
                  data = tomllib.load(f)
              assert data == {"style": "lcd", "rows": "rect"}, data
              '
              touch $out
            '';

          # #1081 review H1, other half: `xdg.systemDirs.config` is a shared
          # home-manager option — a user (or another module) setting it
          # themselves must compose with `nix/hm-module.nix`'s own entry
          # (list concatenation), not conflict with it. A bare
          # `home.sessionVariables.XDG_CONFIG_DIRS = "…";` here — what routing
          # through `trollshellSessionEnv`/`home.sessionVariables` directly
          # would have produced — throws "conflicting definition values" the
          # moment anything else defines that same option, which this fixture
          # reproduces (`/opt/example/etc/xdg`, the exact path the review
          # measured against). Both entries present is the positive half;
          # a clean eval (no `mkForce`, no throw) is the half that used to
          # fail outright.
          hm-module-core-leds-composes-with-xdg-systemdirs =
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
                      enableNixpkgsReleaseCheck = false;
                    };
                    xdg.systemDirs.config = [ "/opt/example/etc/xdg" ];
                    programs.trollshell = {
                      enable = true;
                      package = stubPackage;
                      config.core-leds.style = "crt";
                    };
                  }
                ];
              };
              cfg = hm.config;
              dirs = cfg.xdg.systemDirs.config;
              probe =
                assert builtins.elem "/opt/example/etc/xdg" dirs;
                assert pkgs.lib.any (e: pkgs.lib.hasInfix "trollshell-config-base" e) dirs;
                builtins.deepSeq { inherit dirs; } "ok";
            in
            pkgs.runCommand "trollshell-hm-module-core-leds-composes-with-xdg-systemdirs-check"
              {
                inherit probe;
              }
              ''
                echo "$probe" >/dev/null
                touch $out
              '';

          # The NixOS-module half of the same rendering (#1041): writes
          # straight into /etc/xdg (nix/nixos-module.nix), the default
          # $XDG_CONFIG_DIRS base entry, so no session variable to resolve —
          # `cfg.environment.etc` names the store path directly.
          nixos-module-core-leds =
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
                      config.core-leds = {
                        style = "lcd";
                        rows = "rect";
                      };
                    };
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
              renderedFile = cfg.environment.etc."xdg/trollshell/core-leds.toml".source;
            in
            pkgs.runCommand "trollshell-nixos-module-core-leds-check" { nativeBuildInputs = [ pkgs.python3 ]; }
              ''
                python3 -c '
                import tomllib
                with open("${renderedFile}", "rb") as f:
                    data = tomllib.load(f)
                assert data == {"style": "lcd", "rows": "rect"}, data
                '
                touch $out
              '';

          # The negative half of #1041's module-eval coverage: an unrecognised
          # key under `config.core-leds` must fail at module EVAL, not at
          # runtime when the shell tries to load a nix-store file it cannot
          # parse. `programs.trollshell.config.core-leds` is a typed
          # submodule with no `freeformType` (module-common.nix), so this is
          # really proving the module system's own "does not exist" behaviour
          # applies here — worth pinning anyway, since a later change to a
          # `freeformType`/`extraConfig` escape hatch would silently lose it.
          #
          # Forcing an unrelated top-level attribute (`config.system.
          # stateVersion`) is NOT enough to trigger this, measured: a
          # submodule-typed option's own unmatched-definition check is
          # scoped to that option's nested `evalModules`, run lazily when
          # THAT option's value is actually forced — unlike a genuinely
          # top-level typo (`networking.hostNam`), which the outer
          # `evalModules`' own check catches on almost any access. So this
          # forces `config.programs.trollshell.config.core-leds` itself
          # (`deepSeq`, not just WHNF, since a shallow force can stop short
          # of the bad key).
          #
          # #1081 review M4: `builtins.tryEval` reports only success/failure,
          # never the thrown message — verified, there is no way to recover
          # "The option `…` does not exist"'s text from it — so on its own
          # this check cannot tell "`bogus` is unmatched inside a real
          # `core-leds` option" from "`core-leds` itself doesn't exist any
          # more" (mutation N3: rename the option at
          # `nix/module-common.nix`'s declaration — this check stayed green
          # for the wrong reason). The fix is the **control arm** below:
          # the identical fixture minus `bogus` must come back `success =
          # true`. Only a genuine per-key rejection makes both hold at once —
          # renaming the option away fails the control too.
          nixos-module-core-leds-unknown-key =
            let
              fixture =
                coreLeds:
                (nixpkgs.lib.nixosSystem {
                  inherit system;
                  modules = [
                    self.nixosModules.default
                    {
                      programs.trollshell = {
                        enable = true;
                        package = stubPackage;
                        config.core-leds = coreLeds;
                      };
                      boot.loader.grub.enable = false;
                      fileSystems."/" = {
                        device = "/dev/sda1";
                        fsType = "ext4";
                      };
                      system.stateVersion = "24.11";
                    }
                  ];
                }).config.programs.trollshell.config.core-leds;
              result = builtins.tryEval (builtins.deepSeq (fixture { bogus = "nope"; }) "ok");
              control = builtins.tryEval (builtins.deepSeq (fixture { style = "lcd"; }) "ok");
              probe =
                assert !result.success;
                assert control.success;
                builtins.deepSeq { inherit result control; } "ok";
            in
            pkgs.runCommand "trollshell-nixos-module-core-leds-unknown-key-check" { inherit probe; } ''
              echo "$probe" >/dev/null
              touch $out
            '';

          # #1227 item 1: `programs.trollshell.config.agents` renders
          # `agents.toml` the same way `core-leds` does — this uses the
          # NixOS module (straight into `/etc/xdg`, same as
          # `nixos-module-core-leds` above) rather than home-manager's
          # `XDG_CONFIG_DIRS`-splicing path, since there is nothing
          # `agents`-specific about *which* platform module renders it.
          #
          # Unlike the two `core-leds` checks above, which parse the
          # rendered TOML back with `tomllib` and compare the resulting
          # dict (order- and formatting-insensitive), this asserts the
          # rendered file's BYTES against a checked-in fixture
          # (`crates/hytte-plugin-agents/tests/fixtures/agents-nix-rendered.toml`).
          # That fixture is also fed straight through `AgentsConfig`'s real
          # `Subsystem` reader by a Rust test in the same crate
          # (`config.rs`'s
          # `nix_rendered_fixture_round_trips_through_the_real_reader`), so
          # a renderer change and a fixture change must land in the same
          # commit, or one of the two checks goes red.
          nixos-module-agents-fixture =
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
                      config.agents = {
                        socket = "/run/hyperhive/host.sock";
                        poll_seconds = 5;
                        window.workspace = "agents";
                        display."trollshell-choom" = {
                          label = "choom";
                          project = "viberoot";
                        };
                        display.argus.icon = "starred-symbolic";
                      };
                    };
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
              renderedFile = cfg.environment.etc."xdg/trollshell/agents.toml".source;
              fixture = ./crates/hytte-plugin-agents/tests/fixtures/agents-nix-rendered.toml;
            in
            pkgs.runCommand "trollshell-nixos-module-agents-fixture-check" { } ''
              if ! diff -u ${fixture} ${renderedFile}; then
                echo "rendered agents.toml drifted from the checked-in fixture (${fixture})" >&2
                exit 1
              fi
              touch $out
            '';

          # The home-manager twin of the check above (#1237 review LOW-5).
          # `configFiles` is hand-mirrored in BOTH platform modules —
          # `nix/nixos-module.nix` says so in as many words ("Mirrors
          # `nix/hm-module.nix`'s `configFiles` exactly — keep the two in
          # sync") — and #1227 item 1 changed the inner filter in both, yet
          # only the NixOS render was pinned. Same example, same checked-in
          # fixture, resolved through `configBase` on the trollshell unit's
          # own `XDG_CONFIG_DIRS` (the #1081 review M1 surface) instead of
          # `/etc/xdg`. The two renders are byte-identical today — measured —
          # and that is precisely the property that has to keep holding:
          # without this, a `prune`/`configFiles` edit applied to one module
          # and fumbled in the other ships a home-manager base layer nothing
          # in `checks` ever reads.
          hm-module-agents-fixture =
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
                      enableNixpkgsReleaseCheck = false;
                    };
                    programs.trollshell = {
                      enable = true;
                      package = stubPackage;
                      config.agents = {
                        socket = "/run/hyperhive/host.sock";
                        poll_seconds = 5;
                        window.workspace = "agents";
                        display."trollshell-choom" = {
                          label = "choom";
                          project = "viberoot";
                        };
                        display.argus.icon = "starred-symbolic";
                      };
                    };
                  }
                ];
              };
              cfg = hm.config;
              # Same unwrapping as `hm-module-core-leds` above — see there for
              # why the trollshell UNIT's own `Service.Environment` is the
              # surface to read rather than `home.sessionVariables`.
              environment = cfg.systemd.user.services.trollshell.Service.Environment;
              xdgEntry = pkgs.lib.findFirst (e: pkgs.lib.hasPrefix "\"XDG_CONFIG_DIRS=" e) null environment;
              xdgValue =
                assert xdgEntry != null;
                pkgs.lib.removeSuffix "\"" (pkgs.lib.removePrefix "\"XDG_CONFIG_DIRS=" xdgEntry);
              base = builtins.head (pkgs.lib.splitString ":" xdgValue);
              renderedFile = "${base}/trollshell/agents.toml";
              fixture = ./crates/hytte-plugin-agents/tests/fixtures/agents-nix-rendered.toml;
            in
            pkgs.runCommand "trollshell-hm-module-agents-fixture-check" { } ''
              if ! diff -u ${fixture} ${renderedFile}; then
                echo "home-manager's rendered agents.toml drifted from the checked-in fixture (${fixture}) — it must stay byte-identical to the NixOS module's render, see nixos-module-agents-fixture" >&2
                exit 1
              fi
              touch $out
            '';

          # #1237 review MEDIUM-1: "one store-path file per subsystem that
          # declares at least one non-null field" is the invariant BOTH
          # platform modules state above their `configFiles` — and `agents`
          # broke it, because `display`'s own `default` is `{ }` rather than
          # `null` and `{ }` is not `null`, so the `filtered == { } -> null`
          # guard could never fire again for that subsystem. Measured before
          # the fix: `programs.trollshell.enable = true` with NO `config.agents`
          # at all shipped `/etc/xdg/trollshell/agents.toml` containing a bare
          # `[display]`, on every install. `nix/{hm,nixos}-module.nix`'s
          # `prune` (bottom-up, unlike `lib.filterAttrsRecursive`) is the fix.
          #
          # Deliberately ONE derivation across BOTH modules rather than the
          # `nixos-module-*` / `hm-module-*` pair the names above suggest: the
          # defect lives in the block those two hand-mirror, so pinning it on
          # one platform only is the same gap LOW-5 filed against the fixture
          # check. Each arm carries its own control (the same fixture WITH a
          # field set must still render), so a mutation that simply stopped
          # rendering anything cannot pass.
          modules-agents-absent-when-unset =
            let
              nixosEtc =
                extra:
                (nixpkgs.lib.nixosSystem {
                  inherit system;
                  modules = [
                    self.nixosModules.default
                    {
                      programs.trollshell = {
                        enable = true;
                        package = stubPackage;
                        weather.fallbackCity = "Berlin";
                      };
                      boot.loader.grub.enable = false;
                      fileSystems."/" = {
                        device = "/dev/sda1";
                        fsType = "ext4";
                      };
                      system.stateVersion = "24.11";
                    }
                    extra
                  ];
                }).config.environment.etc;
              hmDirs =
                extra:
                (home-manager.lib.homeManagerConfiguration {
                  inherit pkgs;
                  modules = [
                    self.homeModules.default
                    {
                      home = {
                        username = "alice";
                        homeDirectory = "/home/alice";
                        stateVersion = "24.11";
                        enableNixpkgsReleaseCheck = false;
                      };
                      programs.trollshell = {
                        enable = true;
                        package = stubPackage;
                      };
                    }
                    extra
                  ];
                }).config.xdg.systemDirs.config;

              untouchedEtc = nixosEtc { };
              socketOnlyEtc = nixosEtc { programs.trollshell.config.agents.socket = "/x/y.sock"; };
              socketOnlyFile = socketOnlyEtc."xdg/trollshell/agents.toml".source;

              probe =
                # The defect itself: nothing set, nothing rendered.
                assert !(untouchedEtc ? "xdg/trollshell/agents.toml");
                # `core-leds` was always correct here (every field `null`) and
                # must stay so — the control that says this is about `agents`'
                # `{ }`-defaulted attrset and not about the guard as a whole.
                assert !(untouchedEtc ? "xdg/trollshell/core-leds.toml");
                # …and the file still appears the moment one field is set, so
                # "render nothing, ever" cannot pass this check.
                assert socketOnlyEtc ? "xdg/trollshell/agents.toml";
                # The home-manager arm reads the same `configFiles != { }`
                # gate through `xdg.systemDirs.config` (`nix/hm-module.nix`),
                # so an unpruned `{ display = { }; }` there splices a whole
                # `configBase` onto the session's XDG search path for nothing.
                assert (hmDirs { }) == [ ];
                assert (hmDirs { programs.trollshell.config.agents.socket = "/x/y.sock"; }) != [ ];
                # No `builtins.deepSeq` wrapper here, unlike the sibling
                # checks above: these two bindings are whole `environment.etc`
                # attrsets, and forcing one deeply overflows the evaluator's
                # call stack (measured). The asserts above already force every
                # value this check reads, and `inherit probe` in the
                # derivation's env forces the chain itself.
                "ok";
            in
            pkgs.runCommand "trollshell-modules-agents-absent-when-unset-check" { inherit probe; } ''
              echo "$probe" >/dev/null
              # The other half of MEDIUM-1: a rendered file must not carry a
              # `[display]` heading it was never given entries for. `prune`
              # drops the emptied attrset, so a `socket`-only config renders
              # exactly one line.
              if ! grep -q 'socket = "/x/y.sock"' ${socketOnlyFile}; then
                echo "a socket-only config.agents did not render its socket:" >&2
                cat ${socketOnlyFile} >&2
                exit 1
              fi
              if grep -q 'display' ${socketOnlyFile}; then
                echo "a socket-only config.agents rendered a [display] heading (see #1237 review MEDIUM-1):" >&2
                cat ${socketOnlyFile} >&2
                exit 1
              fi
              # #1306's `[window]` is the same shape as `display` and needs the
              # same pin: its own `default` is `{ }` too, so the emptied-table
              # half of `prune` is the only thing standing between an unset
              # `window` and a bare `[window]` heading on every install. The
              # `display` grep above cannot see it — a second `{ }`-defaulted
              # attrset is a second instance of MEDIUM-1, not a re-test of it.
              if grep -q 'window' ${socketOnlyFile}; then
                echo "a socket-only config.agents rendered a [window] heading (#1306, same shape as #1237 review MEDIUM-1):" >&2
                cat ${socketOnlyFile} >&2
                exit 1
              fi
              touch $out
            '';

          # #1237 review MEDIUM-2: `socket` is judged by a WHOLE-FILE rule on
          # the Rust side (`AgentsConfig::validate` -> `ConfigError::Invalid`
          # -> `load_or_default` discards the entire merged file back to
          # `DEFAULT_TOML`), so a relative path set from nix would silently
          # revert `poll_seconds` and every `[display.*]` entry with it —
          # #1040 V1's named anti-pattern. `nix/module-common.nix` therefore
          # types it `strMatching "^/.+"` rather than an open `str`, and this
          # is the pin.
          #
          # Same tryEval-plus-control shape as
          # `nixos-module-core-leds-unknown-key` above, and for the same
          # reason (#1081 review M4): `builtins.tryEval` reports only
          # success/failure, never the message, so on its own the failing arm
          # cannot tell "`socket` rejected a relative path" from "`socket` no
          # longer exists". Only the control arm — the identical fixture with
          # an absolute path, which must succeed — makes both hold at once.
          nixos-module-agents-relative-socket =
            let
              fixture =
                socket:
                (nixpkgs.lib.nixosSystem {
                  inherit system;
                  modules = [
                    self.nixosModules.default
                    {
                      programs.trollshell = {
                        enable = true;
                        package = stubPackage;
                        weather.fallbackCity = "Berlin";
                        config.agents.socket = socket;
                      };
                      boot.loader.grub.enable = false;
                      fileSystems."/" = {
                        device = "/dev/sda1";
                        fsType = "ext4";
                      };
                      system.stateVersion = "24.11";
                    }
                  ];
                }).config.programs.trollshell.config.agents.socket;
              result = builtins.tryEval (builtins.deepSeq (fixture "run/hyperhive/host.sock") "ok");
              control = builtins.tryEval (builtins.deepSeq (fixture "/run/hyperhive/host.sock") "ok");
              probe =
                assert !result.success;
                assert control.success;
                builtins.deepSeq { inherit result control; } "ok";
            in
            pkgs.runCommand "trollshell-nixos-module-agents-relative-socket-check" { inherit probe; } ''
              echo "$probe" >/dev/null
              touch $out
            '';

          # #1306: `window.workspace` becomes an element of
          # `niri msg action focus-workspace <name>`, and niri's command line
          # is `clap` — so a name starting with `-` is read as a FLAG, not as
          # a workspace. Unlike `socket` above this is not a whole-file rule
          # (the Rust side gives this one key its own verdict and falls back to
          # `DEFAULT_WORKSPACE`), so the eval-time refusal is not preventing a
          # silent revert; it is refusing in front of the operator who typed
          # the value, naming the option path, instead of leaving them a
          # journal line and a focus that quietly went somewhere else.
          #
          # Same tryEval-plus-control shape as the socket check above, and for
          # the same #1081 M4 reason: `tryEval` reports success/failure and
          # never the message, so only the control arm — a legal name, which
          # must succeed — separates "the regex rejected `-hive`" from "the
          # option no longer exists". The control doubles as the pin that an
          # inner space is still accepted: the name travels as its own argv
          # element and niri's kdl quotes it, so whitespace is not this rule's
          # business and a regex that refused it would be wrong in the
          # direction that breaks a working config.
          nixos-module-agents-bad-workspace =
            let
              fixture =
                workspace:
                (nixpkgs.lib.nixosSystem {
                  inherit system;
                  modules = [
                    self.nixosModules.default
                    {
                      programs.trollshell = {
                        enable = true;
                        package = stubPackage;
                        weather.fallbackCity = "Berlin";
                        config.agents.window.workspace = workspace;
                      };
                      boot.loader.grub.enable = false;
                      fileSystems."/" = {
                        device = "/dev/sda1";
                        fsType = "ext4";
                      };
                      system.stateVersion = "24.11";
                    }
                  ];
                }).config.programs.trollshell.config.agents.window.workspace;
              refused = name: builtins.tryEval (builtins.deepSeq (fixture name) "ok");
              probe =
                # Every shape `WindowConfig::workspace_is_usable` refuses.
                assert !(refused "-hive").success;
                assert !(refused "--help").success;
                assert !(refused "").success;
                assert !(refused "   ").success;
                assert !(refused "hi\nthere").success;
                # …and the ones it takes, including the inner space and the
                # built-in default's own spelling.
                assert (refused "hive").success;
                assert (refused "my agents").success;
                assert (refused "hive-2").success;
                builtins.deepSeq { inherit refused; } "ok";
            in
            pkgs.runCommand "trollshell-nixos-module-agents-bad-workspace-check" { inherit probe; } ''
              echo "$probe" >/dev/null
              touch $out
            '';

          # The "lean heavy on nix" counterpart to the Rust ephemeral-EDS
          # harness (#49). Split out to nix/checks/eds-nixos-test.nix
          # (#1102), mirroring how `packages` already lives under
          # `nix/*.nix` — see that file for the full rationale and the test
          # script. This call passes the module fixtures the block used to
          # close over here (`pkgs` is auto-supplied by `callPackage`).
          eds-nixos-test = pkgs.callPackage ./nix/checks/eds-nixos-test.nix {
            inherit probe taskSource calSource;
          };

          # The "lean heavy on nix" harness for the NetworkManager Wi-Fi
          # backend (#96). Split out to nix/checks/wifi-nm-nixos-test.nix
          # (#1102), mirroring how `packages` already lives under
          # `nix/*.nix` — see that file for the full rationale and the test
          # script. This call passes the one module fixture the block used
          # to close over here (`pkgs` is auto-supplied by `callPackage`).
          wifi-nm-nixos-test = pkgs.callPackage ./nix/checks/wifi-nm-nixos-test.nix {
            inherit wifiProbe;
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
