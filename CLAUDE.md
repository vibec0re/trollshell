# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A Rust workspace with two layers:

- **`hytte`** — a library-first toolkit for composing GTK4 + libadwaita + `gtk4-layer-shell` Wayland desktop shells. Split across `crates/hytte-*`.
- **`trollshell`** — the personal shell binary built on `hytte`, targeting the **Niri** compositor.

"Composable, not configurable": there is no config DSL. The shell is wired up in plain Rust in `trollshell/src/main.rs`. **Design lives in GitHub discussions, epics and issues** — a feature is specced on its thread (Annika, Discussion #1063, 2026-09-10), and a build issue links the epic or discussion it came from. `docs/superpowers/{specs,plans}/` is the **April 2026 archive** of the original design (`2026-04-24-hytte-trollshell-design.md`) and the early version specs; consult it for the intent of the subsystems that date from then, but do not add to it — a new design goes on a discussion or an epic, never into that folder.

## Build / run / test

**You must work inside the Nix devShell.** `.envrc` is `use flake` (direnv); if direnv isn't active, run `nix develop` first. The devShell sets env that the build and runtime both require:

- `LD_LIBRARY_PATH`/`LIBCLANG_PATH` so the bindgen consumer (pipewire-sys/libspa-sys) can load libclang. Outside the shell, the build panics with _"a libclang shared library is not loaded on this thread."_
- `XDG_DATA_DIRS` + `GSETTINGS_SCHEMA_DIR` so GTK finds Adwaita symbolic icons and GSettings schemas. Outside the shell, most bar icons render as `image-missing`.

```sh
cargo build --release -p trollshell          # build the binary
cargo run -p trollshell                       # run it (needs a live Niri session: connects to $NIRI_SOCKET)
RUST_LOG=hytte_services=debug,trollshell=debug cargo run -p trollshell   # with logs
nix build                                     # build the packaged binary (.#trollshell)
```

`trollshell` is a real Wayland shell — running it meaningfully requires being **inside a Niri session**. Layer-shell surfaces and most services need live system daemons. (Locking is delegated to `swaylock`/logind, not an in-shell lock screen — see "Deployment & session integration" below.)

### Faster inner loop (devShell only)

Link time dominates the tail of every incremental build (heavy native deps). The devShell wires the **mold** linker by default via `RUSTFLAGS = "-C link-arg=-fuse-ld=mold"` (see `nix/devshell.nix`) — nothing to do, `cargo build`/`clippy`/`test` just link faster. This is deliberately **devShell-only**: the packaged crane build (`nix/package.nix`) has no mold in its sandbox, so a repo-level `.cargo/config.toml` linker setting would break `nix build .#trollshell`.

**sccache** (also in the devShell) caches rustc artifacts across worktrees/branches — handy for the review workflow. It's opt-in to keep the default `cargo` path unsurprising:

```sh
export RUSTC_WRAPPER=sccache        # then build as usual; `sccache --show-stats` to inspect
```

For quick feedback while iterating, `cargo clippy -p <crate> --lib` is much faster than the full `cargo clippy --workspace --all-targets` gate.

### Tests

Tests split into two buckets via the `system-tests` cargo feature (defined in
`hytte-bus`, `hytte-reactive`, `hytte-services`, `hytte-ui`, `trollshell`,
`trollshell-agent-window`, `trollshell-control-center`). **Internals**
(pure logic) run by default; **real-system** tests (those needing a
`dbus-daemon` or a display server) are gated behind the feature so the default
run stays hermetic.

```sh
cargo test                                       # internals only — hermetic, no system deps
cargo test --workspace --features system-tests   # + real-system (dbus-daemon + display)
cargo test -p hytte-services clock               # tests matching a name
xvfb-run cargo test --features system-tests -p hytte-ui   # display tests headless
```

- The real-system tests carry `#[cfg(feature = "system-tests")]` (whole-file
  for integration tests, on the `mod tests` for the GTK unit tests) rather than
  `#[ignore]`, so the default `cargo test` doesn't even compile them.
- `hytte-bus`'s system tests **spawn a real `dbus-daemon`** (one ephemeral
  broker per test; must be on `PATH`). They don't touch the host session bus.
- The GTK-dependent system tests need a display server (`xvfb-run` works).
- `trollshell-agent-window`'s display tests construct a `webkit::WebView`, and
  WebKitGTK sandboxes its subprocesses with `bwrap`, which needs nested user
  namespaces. Without them, constructing the view **aborts the test binary**
  (`bwrap: Can't mount proc…`, SIGABRT) rather than failing a test — so both
  `nix/devshell.nix` and `nix/checks/system-tests.nix` export
  `WEBKIT_DISABLE_SANDBOX_THIS_IS_DANGEROUS=1`, each with the comment saying
  why. Nothing that ships sets it. It buys a constructible widget, not a
  working web process (one still crashes in both), which is why the
  navigation-policy assertion is a live-verify item and not a test — see
  `crates/trollshell-agent-window/src/webview.rs`'s `gtk_tests` module doc.
- Both of those also export `GIO_EXTRA_MODULES` at `glib-networking`'s
  `lib/gio/modules` (#1234). GIO's TLS backend is a **loadable module**, not
  part of libgio, and nothing wraps a `cargo test` — so without it every
  `TlsCertificate::from_file` / `TlsFileDatabase::new` /
  `TlsClientConnection::new` answers _"TLS support is not available"_, which is
  what kept the agent window's whole trust path live-verify-only through
  #1130. It buys `verify.rs`'s `tls_tests`: a real `GTlsServerConnection` on a
  loopback port and the window's own `probe` against it, over checked-in
  fixtures (`crates/trollshell-agent-window/tests/fixtures/tls/`, minted by the
  `generate.sh` beside them). Still **no page is loaded** there — the web
  process dies either way; everything #1234 added runs before WebKit.
- `hytte-services`'s gated test round-trips the NetworkManager secret agent
  (`wifi::nm_agent`'s `GetSecrets`) against a real `dbus-daemon` too.
- The hermetic internals suite (`cargo test --workspace`, deliberately
  **without** `system-tests`) runs as `checks.workspace-tests` (flake.nix),
  gated by `nix flake check` — not by `nix build .#trollshell`, since
  `nix/package.nix` sets `doCheck = false` on the `workspace` derivation
  (#1115; see "Packaging" below).

### Packaging (`nix/package.nix`)

`nix/package.nix` has **one** `craneLib.buildPackage` for the package path — `trollshell-workspace`, built `--workspace --locked` — producing every binary the flake ships, plus, since #1257, a second one: `probes`, a checks-universe compile of the two nixosTest probe examples that no package output reaches (see the carve-out below). `workspace` sets `dontWrapGApps = true`, so its `$out/bin` holds raw, unwrapped ELFs. Every package output is a **slice** of that single derivation, not a second crane call: `nix/plugin.nix` is a `runCommand` that `install -Dm755`s one binary out of `${workspace}/bin/<name>` with no wrapping — not just the bundled widget plugins, but any GTK-free binary the workspace produces (the `hytte-infobroker` CLI, #562; the `hytte-claude-bridge` daemon, #666); the `trollshell` slice (in `nix/package.nix` itself) and `nix/control-center.nix` do the same copy plus a `wrapGAppsHook4` wrap over `workspace.passthru.devInputs.buildInputs`, so the GApps env matches what an in-place compile would have produced.

**Adding a new SHIPPED binary means adding a slice of `workspace`, not a `buildPackage` call.** Before #587 the package path ran 15 crane compile derivations — 13 of which existed purely to copy one binary out — because each `buildPackage` inherited `workspace`'s packed `target` dir as `cargoArtifacts` and hoped cargo would find everything fresh; measured, it didn't, and every one of them recompiled the workspace. #587 collapsed that to one compile plus plain `cp`s specifically so nobody adds a 16th crane call. `probes` is the one deliberate exception, and it ships nothing a `packages.*` output reaches — see below for why it gets its own `buildPackage` instead of being a slice.

Since #1115, `doCheck = false` on the `workspace` derivation: a consumer's `nix build` (this package or any other slice of `workspace`) compiles release only and never runs `cargo test`, so it no longer runs the hermetic internals suite or compiles the dev-dependency graph feeding it — that suite now runs as its own `checks.workspace-tests` (flake.nix), gated by `nix flake check` rather than every build. `buildPackage` still captures binaries out of cargo's JSON build log in a `postBuild` hook that fires at the end of the build phase regardless of `doCheck`, so that capture is unaffected. The deps stage (`craneLib.buildDepsOnly`) shares that same `--workspace --locked` scope; it was wrongly `-p trollshell` before #587, fingerprinting a different feature union than the `--workspace` compile and so caching a dependency graph the compile stage couldn't actually reuse.

The two nixosTest probe binaries (`nix/probe.nix`, `nix/wifi-probe.nix`, #589) are slices too, but — unlike the plugin/shell slices — they `wrapGAppsHook4`-wrap: the EDS VM test needs `GIO_EXTRA_MODULES` for dconf's GSettings backend, which only a GApps wrap injects. Model a new probe-shaped derivation on these two, not on `nix/plugin.nix`. Since #1257 the binary they slice is **not** `workspace`: the two probes are `--example` targets, and until #1257 building them rode a `postInstall` on `workspace` itself, which meant every package build paid to compile hytte-ecal's and hytte-services' dev-dependency closures too, on `cargoArtifactsBinOnly` — the cache `workspace` exists specifically to hold none of that. `probes` (same file, next to `workspace`) is a separate `craneLib.buildPackage` on `cargoArtifacts` instead — the dev-deps cache the checks (`clippy`/`system-tests`/`workspace-tests`) already share — so the two probe slices now take both `workspace` (for `passthru.devInputs`, the GApps wrap env) and `probes` (for the binary itself), and no package build reaches the extra compile. `checks.workspace-ships-no-probes` pins it: `workspace`'s `$out/bin` must carry neither example, and its own `.drv` must not reference `--example`.

### CI (`nix flake check`)

Since #1115 the package build no longer runs any tests at all (see
"Packaging" above), so the flake's `checks` output (`flake.nix`) is where
every test suite runs, plus a fair bit more — since #1102 the three heaviest
check derivations (`system-tests`, `eds-nixos-test`, `wifi-nm-nixos-test`)
live in their own `callPackage`-able files under `nix/checks/`, the same
convention `packages` already follows in `nix/*.nix`, while the rest stay
inline in `flake.nix` as one-liners.

- `checks.workspace-tests` runs the hermetic internals suite (still
  deliberately without `system-tests`) — the same suite the package build's
  `doCheck` used to run on every `nix build`, now gated here instead
  (#1115).
- `cargo clippy --workspace --all-targets --features system-tests -- -D warnings`
  (the `system-tests` feature is enabled here so the gated integration tests
  and GTK `mod tests` blocks are lint-checked too, not just compiled once and
  forgotten).
- `treefmt` formatting (`nix/treefmt.nix`) — this is what `nix fmt` runs
  locally.
- The full `system-tests` cargo-feature bucket, run for real
  (`cargo test --workspace --features system-tests` under `xvfb-run`, with a
  `dbus-daemon` on `PATH`) — this is the _only_ place those tests run, since
  the package build's `doCheck` deliberately skips them.
- `nixosModules.default` / `homeModules.default` module-eval checks (force the
  systemd units, session vars, and assertion predicates the modules generate).
- Two `nixosTest` VMs: `eds-nixos-test` (evolution-data-server / `hytte-ecal`
  end-to-end) and `wifi-nm-nixos-test` (the NetworkManager Wi-Fi backend
  against simulated `mac80211_hwsim` radios).
- Since #449: actually **building** `packages.{trollshell,trollshell-control-center}`
  as part of `checks` — before that, `nix flake check` could stay green while
  `nix build .#trollshell` (or the control-center) was broken, because `check`
  only builds what's listed under `checks`, not `packages`.
- `bind-pins` (#831): a source scan (`nix/lint-bind-pins.py`, run by a
  `runCommand` — no compile, so it goes red in seconds) that fails the build if
  a `bind*` call site discards the closure's own widget parameter and uses a
  captured strong clone of the same widget instead, which pins the widget for
  the binding's lifetime and defeats the `WeakRef` contract in
  `crates/hytte-reactive/src/bind.rs` (#224). Not a clippy lint because the
  pattern is cross-statement and repo-specific; not a unit test because ten of
  the twelve sites #831 found can't be constructed without a registered
  `Registry`. Run it by hand from the repo root with
  `nix shell nixpkgs#python3 --command python3 nix/lint-bind-pins.py` — the
  `nix shell` is not optional: **`python3` is deliberately not on the devShell
  PATH**, so a bare `python3 …` (or `nix develop --command python3 …`) is
  `command not found`, which a script run for its side effects can swallow. The
  script's header documents the deliberate carve-out (capturing a _different_
  widget is correct) and why it paren/brace-matches instead of using a regex —
  read it before changing it. Since #1259 (the #1244 sidebar leak this scan
  reported `0 pin(s)` on both sides of) it also discovers connect-helper
  functions like `hytte::ui::on_surface_ready`/`on_map_or_now` — ones that
  wire a widget to a handler on the caller's behalf rather than through a
  `connect_*` receiver call — and flags the same strong-clone-into-a-discarded-
  closure-parameter shape at a call to one of them.
- `lints-tables` (#1179): the same shape for the three hand-mirrored `[lints]`
  tables — `nix/lint-lints-tables.py` parses the root `[workspace.lints]` and
  both `unsafe` islands' copies (`crates/hytte-ecal/Cargo.toml`,
  `crates/hytte-gl/Cargo.toml`) and fails unless they agree on every entry
  except the one `unsafe_code` line, which must read `"allow"` on an island
  and `"forbid"` at root. Nothing enforced the "keep the three tables in sync"
  comment before: deleting `pedantic` from an island leaves the clippy check
  above and the whole of `nix flake check` green, so one of the two crates
  where `unsafe` is legal silently stops being pedantic-checked. It asserts the other direction too — every non-island
  member inherits with `[lints] workspace = true` — so a **third** island
  cannot appear unnoticed. `hytte-ecal`'s five documented FFI-only `allow`s
  (`missing_safety_doc`, `doc_markdown`, `must_use_candidate`, `ref_as_ptr`,
  `borrow_as_ptr`) are declared in the script's `EXTRA_ALLOWS` rather than
  waved through, so adding a sixth means editing the script in the same commit
  — which is the review moment an "anything extra is fine" rule would skip.
  Run it by hand with
  `nix shell nixpkgs#python3 --command python3 nix/lint-lints-tables.py` — the
  `nix shell` is not optional, for the `bind-pins` reason above.
- `glsl` (#893 stage B): the same shape for the preem GL renderer's shaders —
  `nix/lint-glsl.py` assembles each `trollshell/src/plugins/preem_gl/*.{vert,frag}`
  the way `Program::compile` does (the `GLSL_HEADER` const is read out of
  `crates/hytte-ui/src/gl_surface.rs`, the blur's `BLUR_DIR` splice out of
  `program.rs`'s `concat!`) and runs `glslangValidator` over it. Nothing else in
  the tree looks inside those files — they are `include_str!`'d `&'static str`s
  until a driver compiles them, and no check here has one — so without this a
  typo'd identifier ships green and surfaces as a blank chip on glass. The
  design spec named **naga** for this row; measured, naga 26's GLSL frontend
  rejects the whole ES profile (`#version 300/310/320 es` all
  `InvalidVersion` + `InvalidProfile("es")`), so this is `glslang` instead —
  which costs zero `Cargo.lock` entries, since it is invoked as a binary at
  build time rather than linked into the shell. Run it by hand with
  `nix shell nixpkgs#python3 nixpkgs#glslang --command python3 nix/lint-glsl.py`
  — `glslang` is in the devShell, **`python3` is not** (deliberately), so the
  `nix shell` prefix is what makes this run at all. Since #893 it also compiles the **shader
  widget**'s vertex stage and every plugin-supplied fragment _body_ shipped in
  the tree (`crates/*/shaders/*.frag`, globbed rather than listed so a new
  plugin's bodies are covered the day they land), each with the
  `SHADER_PREAMBLE` interface declarations spliced in front — read out of
  `crates/hytte-ui/src/shader_surface.rs`, so a change to the published uniform
  contract changes what CI validates in the same commit. Since #1325 it also
  whole-word-scans every one of those bodies for the handful of GLSL ES 3.00
  §3.7 words `glslangValidator` accepts as identifiers but Mesa's ESSL lexer
  refuses (`packed`, `row_major`, `column_major`) — a driver-only false-green
  `glslangValidator` alone cannot catch — proved by its own `--self-test` arm.
  A plugin's _runtime_ source is deliberately not validated anywhere; see the
  trust boundary below.
- `lint-manifest-id` (#1372, from the #1365 adversarial review's L5): the
  same shape for `crates/trollshell-control-center/src/plugins_tab.rs`'s
  `manifest_id_of_exec`, a hand transcription of this file's own
  `inferManifestId` (above) — the rule that turns a plugin's `exec` basename
  into the id it registers under, which the control-center reads back to
  decide which settings-form family (if any) a plugin owns. Nothing checked
  the two agreed before this: a plugin whose family lookup depends on the id
  (e.g. `stats`) could silently stop mounting a form if either side drifted,
  with no error, since "no family" is a legitimate answer. Both sides are
  graded against one shared table, `nix/manifest-id-cases.txt` (its own
  header has the provenance of the "expected id" column — a real evaluation
  of `inferManifestId`, not a second transcription): `nix/lint-manifest-id.py`
  re-parses the rule's two prefix literals out of `nix/module-common.nix` on
  every run (so a changed literal is caught as real per-row drift, not
  silently absorbed) and checks every row, while the Rust side is graded
  separately by `checks.workspace-tests`'
  `manifest_id_of_exec_matches_the_shared_table` test reading the identical
  file. Run the nix-side scan by hand with
  `nix shell nixpkgs#python3 --command python3 nix/lint-manifest-id.py` — the
  `nix shell` is not optional, for the `bind-pins` reason above.
- `config-vocab` (#1375, #888 P3): the same shape for `nix/module-common.nix`'s
  hand-mirrored config vocabulary — `nix/lint-config-vocab.py` parses all
  four `Subsystem` families' own `SCHEMA` consts (two in
  `hytte-config-families`, one apiece in `hytte-plugin-agents` and
  `hytte-plugin-stats`) and gates `programs.trollshell.config.{core-leds,
  agents}`'s leaf set, bounds/enums and each option's `description` first
  sentence against the two of those four that have a nix block today
  (`workspaces`/`stats` are parsed and counted but have none yet, reported
  `skipped: no nix surface`), plus the unrelated, unchanged `places` and
  `plugins.<id>.mount` mirrors it also carries. Run it by hand with
  `nix shell nixpkgs#python3 --command python3 nix/lint-config-vocab.py`
  (`--self-test` runs just the fixture and mutation layers) — the `nix shell`
  is not optional, for the `bind-pins` reason above.
- `rustdoc` (#1328): `cargo doc --workspace --no-deps` with
  `RUSTDOCFLAGS="-D warnings"`, on the `workspace-tests` precedent above —
  its own leaf row, not folded into it, sharing `cargoArtifacts` rather than
  compiling a fresh dependency graph. Nothing ran `cargo doc` in CI before
  this: PR #1322 shipped ten `private_intra_doc_links` warnings on
  `hytte-plugin`'s public `effective_mount`/`effective_mount_from` docs
  unnoticed, and a rustdoc warning on a `pub` item is a broken link on the
  one page a plugin author actually reads for the SDK. `private_intra_doc_links`
  (a public item's doc linking a private one) and `broken_intra_doc_links` (an
  unresolvable path, an ambiguous `mod`-vs-`fn`/`macro` name, or an invalid
  anchor) are fixed in the doc comment itself — linking an already-public item
  instead, an explicit `[`name`](Self::name)`-style path, a `mod@`/disambiguated
  path, or dropping the markdown link syntax to plain code when nothing public
  is a legitimate target — never with `#[allow]` or `#[doc(hidden)]`. Run it by
  hand from the devShell with
  `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`.
- Since #1036, the `system-tests` check's closure carries `mesa` (llvmpipe) and
  its `preCheck` exports the software-GL env plus `TROLLSHELL_REQUIRE_GL=1`,
  so the three GL-context tests in `hytte-ui` (`gl_surface.rs`) actually run
  under a real `GdkGLContext` there instead of skipping — `TROLLSHELL_REQUIRE_GL=1`
  turns a skip into a failure the same way `flake.nix`'s `preCheck` already
  uses `TROLLSHELL_REQUIRE_ICON_THEME` to do that for the icon-theme test.
  Since #1082, the same closure also carries `pkgs.systemd` (marginal cost: 1
  new store path, `systemd-*-dev`, ~263 KiB — the full `systemd` runtime
  containing `systemd-run` was already pulled in transitively by pipewire/
  evolution-data-server/gtk4) so `systemd-run` is on `$PATH` for the plugin
  host's detached-launch tests (`trollshell/src/plugins/tests.rs`), and
  `TROLLSHELL_REQUIRE_SYSTEMD_RUN=1` turns a missing `systemd-run` there into
  a failure the same way, gating the one test that has a skip branch
  (`detached_launch_falls_back_without_a_user_manager`) — its siblings accept
  either `LaunchReport` fallback and need no gate.
  Since #1080, the same `checkPhaseCargoCommand` also builds and runs
  `trollshell/examples/preem_gl_diff` — the #893 stage B CPU/GL parity
  harness — through that same llvmpipe context, with `TROLLSHELL_PARITY_EXACT=1`
  pinning it to the bit-exact result (`max |Δ| 0` on every channel, all
  twelve cases) llvmpipe has measured since #1078, tighter than the on-glass
  ceiling (mean 2 / p99 8 / max 32, #893). Its per-case evidence images go to
  `$out/parity` (`PREEM_GL_DIFF_OUT`) instead of the default `gates/`, so a
  build's own output carries them. No new closure inputs — it runs through
  the same `mesa`/`xvfb-run` the GL tests already pulled in.

Since #1231 the workflow (`.github/workflows/nix-flake-check.yml`) no longer
runs those checks as one `nix flake check` on one runner. Five of them are
each a full workspace compile (`packages.trollshell`'s release build off the bin-only
deps cache; `workspace-tests`, `system-tests`, `clippy` and `rustdoc` off the
dev-graph one), and running three of them concurrently on one 4-core runner
made the wall time their _sum_ — 51, 73 and 86+ min against the 75-minute
bound #1012 sized and #1230 had to raise. So each heavy check now
`nix build`s on its own runner: `packages` (every shipped binary is a slice
of the one `workspace` compile, so one job builds all 19 plus
`shell-has-no-web-engine` and `workspace-ships-no-probes`), `workspace-tests`,
`system-tests`, `clippy`, `rustdoc`, and `nixos-tests` (both VMs in one job —
they share the single `probes` compile, which is the cost; the VMs themselves
are ~2 min each). Everything else — treefmt, the five source scans,
`options-doc`, the 26 module evals — is one `cheap` job. **The required check
is the aggregate job named `flake-check`**, which `needs:` every job above and
is red if any failed, was cancelled or was skipped; that name is what branch
protection and the merge poller read, so it does not move. It is only the
_builds_ that moved out: the `cheap` job still runs
`nix flake check --no-build --all-systems` for the evaluation sweep and the
per-output-type checks (`nixosModules.default` among them) that nothing else
does. What a hand-written build list would lose is "everything in `checks`
runs", so a step beside it guards the layout against rot: it diffs
`nix flake show`'s check list against every `checks.<system>.<name>`
installable the workflow actually builds — read out of the workflow file
itself, so it cannot drift from what the jobs run — and fails in either
direction, so a new check nobody adds to a job cannot silently go untested.
What the split costs is CPU, not wall time:
runners share no Nix store, so each job compiles the deps closure it needs
rather than one run compiling it once (#1268). A cross-run binary cache
(#1231 item 2) is what turns those into hits, and it needs a token.

### Lint — strict, treat as the gate

The workspace lint config (`Cargo.toml`) is deliberately severe; a violation fails `cargo check`, not just clippy:

- `unsafe_code = "forbid"` workspace-wide. **Only `hytte-ecal` and `hytte-gl`** override this — the two islands (FFI to libecal; OpenGL entry points), each confining its unsafety to safe wrappers and each hand-mirroring the root lints table because workspace-lints inheritance is all-or-nothing. Keep the three tables in sync — `checks.lints-tables` (#1179) fails if you don't.
- clippy `all` **and** `pedantic` at `deny`. Code must be pedantic-clean.
- `disallowed_methods`: `zbus::Connection::session`/`::system` are **banned** (see `clippy.toml`). All D-Bus access goes through the `hytte-bus` primitives, never a raw zbus connection.

```sh
cargo clippy --workspace --all-targets        # must be clean
cargo fmt --all
```

Edition 2024, MSRV **1.92** (`rust-version.workspace = true` on every member since #453 — #1184 closes the one gap, `hytte-plugin-infobroker`, that had drifted from the rule; wiring up the inheritance is what surfaced `clippy::incompatible_msrv` violations against the previously-fictional 1.85 and forced the first bump; not independently CI-gated beyond that clippy check — the devShell/crane toolchain floats on nixpkgs' current rustc, ~1.95). **1.92 rather than 1.91 since #1130**, and unlike the previous value this one is enforced by cargo rather than merely declared: `webkit6` pulls `soup3`/`soup3-sys` 0.9.0, which declare `rust-version = "1.92"`, and cargo hard-errors at _build_ time on a toolchain below a package's declared floor — so a 1.91 toolchain cannot build this workspace at all. (`resolver.incompatible-rust-versions` is a red herring there: it steers version selection while a lockfile is generated, and this one is pinned.) The nix build and devShell use nixpkgs' rust toolchain (via crane); there is no `rust-toolchain.toml` pin.

Shared dependency versions/feature baselines live in the root `Cargo.toml`'s `[workspace.dependencies]`; members inherit with `dep.workspace = true` rather than hand-repinning (#453). Every dependency comes from crates.io — the workspace has **no** git dependencies. `hive-claude` (`crates/hytte-claude-bridge` only) was the last one: a forge rev pin (#666) that crane resolved at eval time via `builtins.fetchGit`, rev-reproducible but not hash-pinned/substitutable the way a locked flake input is, so a cold `nix flake check` depended on that forge being reachable. #757 took the published 0.1.0 instead, closing #671 — keep it that way, and reach for a flake input over a bare rev pin if a git dependency is ever unavoidable. `deny.toml` (repo root) holds a `cargo-deny` advisories+licenses config, run locally — it isn't wired into `nix flake check` (the advisory-db fetch needs network, which sandboxed nix builds don't have):

```sh
nix shell nixpkgs#cargo-deny --command cargo-deny check
```

## Architecture — the reactive core

The whole design avoids threading `Arc<Mutex<…>>` through widgets by splitting **handles** from **work**:

1. **Handles** — `Mutable<T>` / `MutableVec<T>` from `futures-signals` — live in a **thread-local `Registry`** on the GTK main thread (`hytte-reactive/src/registry.rs`), keyed by `TypeId`.
2. **Work** — D-Bus, sockets, PipeWire — runs on a **process-wide multi-thread tokio runtime** (`hytte-reactive/src/runtime.rs`, `runtime::handle()`).
3. tokio tasks update state by calling `mutable.set(…)` directly (`Mutable` is `Send + Sync`); the registry itself never crosses threads.
4. Widgets subscribe with `bind(signal, &widget, |w, v| …)` (and `bind_text`/`bind_visible`/`bind_class`/`bind_two_way`), which spawns an apply-loop on `glib::MainContext` (GTK main thread). See `hytte-reactive/src/bind.rs`.

The shell author never sees an `Arc`, `Mutex`, or a handle — only free functions returning `impl Signal`.

### The service pattern (every `hytte-services` module follows it)

```rust
pub struct FooService;            // implements hytte_reactive::Service
impl Service for FooService {
    type Handles = FooHandles;    // a struct of Mutable<…> fields
    fn start(self, rt: &tokio::runtime::Handle) -> Self::Handles { … }  // spawn tokio tasks, return handles
}
pub fn service() -> FooService { FooService }                 // registered via App::with(…)
pub fn some_state() -> impl Signal<Item = …> {                // subscribe accessor
    registry::with(|r| r.get::<FooHandles>().expect("foo::service() not registered").field.signal_cloned())
}
pub fn do_thing(arg: …) { … }                                 // fire-and-forget command (e.g. niri::focus_workspace)
```

`clock.rs` is the minimal example; `upower.rs` is the canonical D-Bus example (one `hytte_bus::property` subscription per field, each updating its slice of the shared state). Accessors `.expect(...)` if the service wasn't registered — so a widget pulling `foo::state()` requires `App::with(foo::service())` in `main.rs`.

### system-daemon-as-state-store

Services are **thin async clients to persistent system daemons** (systemd-networkd, NetworkManager, BlueZ, PipeWire, UPower, logind, niri-ipc, iwd, evolution-data-server, …). Persistent state lives in the daemon, not in `hytte`, so restarting `trollshell` during dev reconnects **without losing system state**. This is a core design constraint — keep new state in the daemon where possible. The network stack sources from **both** networkd (link/route state, `networkd.rs`) and NetworkManager (`networkd_nm.rs`, `wifi_nm.rs`), with an NM secret agent (`wifi/nm_agent.rs`) answering Wi-Fi/VPN `GetSecrets`.

**Exception:** `notifications` registers itself as the `org.freedesktop.Notifications` daemon (a session singleton), so any other notification daemon (mako/dunst) must be disabled.

### Crate graph

```
hytte-reactive   ← base: Service trait, Registry, tokio runtime, bind() helpers
  ↑   ↑   ↑
hytte-ui  hytte-services  hytte-bus
hytte-ui          → App/AppBuilder (wraps adw::Application), Bar, LayerWindow, Popup, Monitor; layer-shell; default stylesheet
hytte-bus         → shared D-Bus layer: call / property / proxy / signals / own_name builders over pooled session+system connections
hytte-services    → the service modules (clients to daemons)
hytte-config      → GTK-free leaf (serde/serde_ignored/toml/toml_edit/tracing only): the `places.toml` schema + validation + its format-preserving `toml_edit` writer, plus the atomic `~/.config/trollshell/*` write helper (ex `hytte-services::config_file`, aliased back as `pub(crate) use hytte_config::file as config_file` so in-crate call sites still read `config_file::…`). Consumed by BOTH `hytte-services` and `trollshell-control-center` — the first crate the shell's service layer and the companion app share, which is exactly why it exists: `places.toml` has two editors (#640/#703, the file stays hand-editable) and they must agree byte for byte rather than each carrying its own serialisation path. Since #868 it also holds the **config layering** #866 settled: `xdg` (the `XDG_CONFIG_DIRS` base → `XDG_CONFIG_HOME` overlay search path, plus the `XDG_STATE_HOME` path so state never shares a directory with config), `merge` (the four rules — scalars overlay-wins-when-present with `_unset` spelling the null TOML lacks, tables deep-merge, arrays **replace**, and unknown keys warn via `serde_ignored` rather than fail), `subsystem` (declare a type + a name + a documented `DEFAULT_TOML`, inherit the reader/validator/format-preserving writer) and `state`. Since #1227 rule 1 has **one exception, per key**: a base layer names the keys it pins in `_locked` (nix renders one entry per option leaf the operator actually set, so an unset option locks nothing and there is no knob — Annika on #866, 2026-09-15), the rule is that the **union of every base layer's `_locked` binds the overlay and nothing else** — so `merge_all_locked` takes the bases and the overlay as two arguments rather than guessing the split from a position, base layers still fold among themselves by XDG precedence with no enforcement between them (the earlier "a lock binds every layer above it" reading inverted the search path, since the fold order is the search path reversed — #1331 review), and the overlay's own marker binds nothing and is not in the returned set, because that set is what the fold ENFORCED and a save driven by it would otherwise drop the operator's own value. An override — an `_unset` included — is refused and reported **once** per key as a `Loaded::lock_findings` entry plus a journal line naming the subsystem, the key, the file to edit and the base the kept value came from; a marker that pins nothing is reported too, whether its shape is unreadable (`malformed_locked`) or it names a key its own layer does not set (`inert_locked`), neither of which nix can emit. `Loaded::locked`/`is_locked` + `save_overlay_to_locked` are how an editor greys a row and keeps a save from writing the nix value into the operator's own file; `assemble` reads its last layer as the overlay and `assemble_base_layers` says there is none, which is what `load_from` passes when the overlay file does not exist yet. Since #1044 `subsystem` also carries everything the `core-leds.toml` pilot (#869/#1040) had grown in the shell, so family #2 is a declaration plus a wiring line: the per-key tolerance on the trait itself (`type Resolved` + `fn parsed` returning `(Resolved, Vec<InvalidValue>)`, so a bad value costs its own key and not the whole file), `subsystem::env` (the `EnvKnob` table, the `Deprecations` gate and the three sentences a migrated `TROLLSHELL_*` variable produces — deliberately a per-key `env::key<T>` fan-out, not a table of homogeneous triples, which #1040's second review measured cannot exist), and `subsystem::watch` (the `(mtime, len)` layer poller, stamp-before-load). Two **cargo features**, both off by default and both there to keep the dependency list above true for the control-center: `watch` pulls `tokio` + `futures-signals` (the shell's dependency line enables it, the control-center's does not — so a settings app never grows a runtime it does not drive), and `test-support` exposes `test_support` (the process-wide tracing global default #1022 needs, the capture harness and the scratch-`Overlay`), reached only through **dev**-dependencies from `trollshell` and `hytte-reactive`, so no shipped rlib carries a `set_global_default`. `places` predates all of it and still goes through none of `subsystem` — it keeps its own reader, its own schema and its own format-preserving writer, and `crates/hytte-config/tests/places_byte_identical.rs` pins that writer's bytes against a recording taken from `origin/main` before the layering existed, so a change that moves the two editors apart fails there. Since #1227 item 2 it does **read** through `xdg` + `merge`, though: `places::assemble_places`/`load_layered` fold `DEFAULT_CONFIG`, the `XDG_CONFIG_DIRS` base layers and the overlay with the same four rules and the same `_locked` set, borrowing `subsystem`'s `Finding`/`locked_marker_findings`/`shadowed_findings` so the journal wording is shared rather than re-rendered. Three things are `places`-specific and argued in its own module doc: the overlay slot is `places::config_path()` (`$HOME/.config`, what the writer has always resolved) rather than `xdg::overlay_path`, so the reader reads the file the writer writes; `place` is an **array**, so rule 3 means a layer supplies the whole list or none of it and `_locked = ["place"]` is one atomic leaf; and because a locked array cannot be partially overridden, `check_unlocked` refuses a save up front with `PlacesError::Locked` (the invariant `save_overlay_to_locked` has for the `Subsystem` families) and the control-center's Places tab shows the entries read-only with "set in nix" — the first editable surface #1331 had nowhere to grey a row on
hytte-sensors     → GTK-free leaf (#1249, P0 of the #1248 epic): the procfs/sysfs samplers — CPU load/clock, memory, network I/O + TCP socket counts, disk I/O + usage, GPU state, process count — extracted byte-for-byte out of `hytte-services::sensors` on the `hytte-preem` precedent (#859). Pure `std` plus `nix` (`statvfs` for disk usage) and, dev-only, `tempfile`; carries the `read_*`/`compute_*` functions, their pure data shapes (`CpuLoad`, `CpuFreq`, `Memory`, `NetIo`, `DiskUsage`, `GpuState`, …) and their unit tests. `hytte-services::sensors` keeps everything that needs `Mutable`/a tokio runtime — the `Service`/`SensorsHandles` wrapper, the 1 Hz tick loop, the sparkline history accumulators, the `AsyncFd` mount-table watcher — and re-exports the moved data shapes at their old `hytte_services::sensors::…` path so `trollshell`'s widgets/panels compile unchanged. Exists because a plugin cannot subscribe to `hytte-services` directly: #1248's later phases put the same sampler behind a `hytte-plugin-stats` (#1250), which needs these reads without linking GTK, D-Bus, or the reactive registry
hytte-ecal        → hand-written FFI to evolution-data-server (libecal); one of TWO crates allowed `unsafe`
hytte-gl          → the **second `unsafe` island** (#893 stage B), on the `hytte-ecal` precedent: `Cargo.toml` hand-mirrors the root lints table with `unsafe_code = "allow"` because workspace-lints inheritance is all-or-nothing. It exists because the workspace `forbid`s unsafe and *every* raw-GL binding marks each entry point `unsafe fn`, so nothing could issue a draw call — which is why stage A's `gl_probe` deliberately measures GTK's integration cost without touching GL. GTK-free and tiny: one safe RAII type per GL object (program with the driver's info log handed back, immutable-storage texture, FBO, VAO), plus blend/viewport/clear and two attribute-less draws. Adds **no** resolved package — `gl 0.14.0` is already in `Cargo.lock` via `gdk4`, `libloading` via `clang-sys` — and since #1067 resolves its entry points through **glvnd's `eglGetProcAddress`** (`dlopen("libEGL.so.1")`, already mapped by GTK's own closure via `libgstgl`), whose `libGLdispatch` stubs route to the vendor of the **thread-current** context on every call, so one process-wide `gl::load_with` serves every `GdkGLContext` (#886). libepoxy is only the **fallback**, and the reason #1067 existed: it exports 3403 `epoxy_gl*` *variables* (`D`, function pointers) and **zero** plain `gl*` functions — the unprefixed spelling is a `#define` in epoxy's header that never reaches a symbol table — so the original loader's plain-name lookup resolved nothing on any NixOS machine and the GL renderer silently fell back to the CPU kit for the whole life of #893 stage B. The epoxy route needs one extra deref (`libloading::Symbol<T>` is the dlsym *address*); plain names from the process image are the last resort. `crates/hytte-gl/src/loader.rs`'s tests pin all three routes hermetically — no display, no driver, no GL context — via a `gdk4-sys` **dev**-dependency taken purely for its link line, which is what maps libepoxy/libEGL into the test binary. Consumed by `hytte-ui`'s `gl_surface` only; `trollshell` reaches GL through that widget and links this crate only as a dev-dependency, for the `preem_gl_diff` parity harness
hytte-ai-providers → shared OpenAI-compatible chat client + provider config + env-based provider-key loader, used by plugins that talk to an LLM (e.g. hytte-plugin-pet) — and, since #993, by `hytte-claude-bridge` itself, for the socket-path constants only. A base URL may name a **Unix socket** (`unix://…`, `src/unix.rs`) as well as an `http(s)://` endpoint; the socket arm plugs a `UnixStream` into `ureq`'s `unversioned` connector/resolver seams rather than hand-rolling a second HTTP client, so a provider's request bytes are identical either way. Semver-exempt API by ureq's own declaration — a minor bump breaks the build, which CI catches; it adds no resolved package. `$XDG_RUNTIME_DIR` is the one token the URL parser expands (nix cannot know `/run/user/<uid>` at eval time), and an unresolvable socket URL is an error, never a fall back to a port
hytte             → umbrella: re-exports {bus, reactive, services, ui} + a `prelude`
trollshell        → the binary; depends on `hytte` — plus, since #857, `hytte-preem` directly: the Stats drawer's per-core LED panel built the kit's `LedMatrix` and, until #1157, rasterised it in-process into a `hytte::ui::PixelSurface`. The dep is unchanged by that retirement and always was more than the rasterise call: the shell reads its widgets' **geometry and palette** out of the kit (`LedMatrix`, `Gauge::dial`, `palette_snapshot`, `with_pins`) and hands those to the shaders. That is the kit, NOT the plugin SDK; the shell still never links `hytte-plugin` — and, since #1040, `hytte-config` directly too: `trollshell/src/config/*` declares the shell's own config-file schemas (`core-leds.toml` the pilot) over `hytte-config`'s layering/writer, adding no new resolved package (already a `hytte-services`/`trollshell-control-center` dependency)
trollshell-control-center → separate windowed GTK4/libadwaita companion app (#390/#399); talks to the running shell over its own `Control` D-Bus endpoint (trollshell/src/control.rs), never linked into the shell. It cannot link `hytte-services` either (that would drag libpipewire + evolution-data-server into a settings app), but since #640 it does link `hytte-config` — a shared GTK-free *leaf library*, not a runtime link to the shell: its Places tab reads and writes `places.toml` directly, through the same writer `hytte-services` uses, so the editor keeps working while the shell is down and the shell's existing mtime poll picks a save up with no new D-Bus surface. Since #947 P4 it also links `hytte-plugin-agents` as a library, on the same argument and the `trollshell-agent-window` precedent: its read-only **Agents tab** (`agents_tab.rs`, spec §10) polls `host.sock` directly through that crate's wire mirror, client, `Status` collapse, `group` ordering and `agents.toml`, because three readers of one socket must agree byte for byte about its verbs and a third mirror would be a third thing to update when hyperhive moves. That tab adds **no** `Control` method and no service — it is the first thing in this app that talks to something other than the shell, and it keeps working (saying so, naming the socket) while both the shell and the hive are down
hytte-claude-bridge → GTK-free daemon (#666/#584) that **also wears the plugin hat** since #866 (Annika's call on that thread): it deps `hytte-plugin` — the one arrow out of this entry, pointing down into the plugin-side block below — while still nothing in the tree links *it*. Its primary duty is one HTTP route (`POST /v1/chat/completions`) that the LLM plugins (pet, caw) consume purely as a `Provider` base URL, so neither has ever needed a code change — but since #993 it is served on a **same-uid Unix socket** (`$XDG_RUNTIME_DIR/trollshell/claude-bridge.sock`, `0600` in a `0700` dir, `src/socket.rs` mirroring `trollshell::plugins::listener`'s lock→probe→bind sequence), not `127.0.0.1:8787`. The route is keyless and spends the owner's Claude subscription, so reachability *is* the authorization boundary, and TCP loopback carries no file mode: every other local uid and every host-netns container could bill to it. `CLAUDE_BRIDGE_PORT` and `claudeBridge.port` are gone with the port; the path is not configurable, there is no fallback if `XDG_RUNTIME_DIR` is unset, and the plugins reach it through `hytte-ai-providers`' `unix://` transport (`crates/hytte-ai-providers/src/unix.rs` — a `UnixStream` under `ureq`'s `unversioned` connector seam, so the bytes on the wire are still exactly what the TCP client sent). That is also why the bridge now deps `hytte-ai-providers`: for `bridge_socket_path_in`, the one definition of a path two ends must agree on byte-for-byte. The second hat is `hytte-plugin-infobroker`'s shape — a real daemon that also paints a chip — and it is what lets the bridge ride `programs.trollshell.plugins` (the launcher, the control-center's Plugins tab, #392's keyring injection) instead of a hand-declared systemd unit; `nix/hm-module.nix` renders `plugins.claude-bridge` and declares no `trollshell-claude-bridge` unit any more. **Where it differs from the infobroker is which duty owns the process, and that is deliberate**: the infobroker starts its socket server from `sources()`, so the server lives one plugin session; the bridge's clients are other plugins making paid calls, so `main` binds and spawns the HTTP listener on its own multi-thread runtime *before* handing the main thread to `hytte_plugin::run` (which builds a current-thread runtime and blocks forever — hence `main` is not `#[tokio::main]`). Its no-`XDG_RUNTIME_DIR` park (park on the HTTP runtime rather than let the SDK exit the process) is unreachable since #993 — the API needs that variable too, so `start` refuses first — and is kept only as a backstop against the two socket-path resolvers ever disagreeing, since the API must not go down with the chip. The two runtimes share only `src/status.rs`'s atomics and, since #1236, `src/usage.rs`'s board. Linking the SDK drags no GUI closure (proto + hytte-preem + tokio) and added no `Cargo.lock` entry. Still the sole consumer of `hive-claude`, which was the workspace's last git dependency until #757 moved it to crates.io (see "Lint" above).

— plugin side (#35 frontend B; out-of-process, NEVER links the shell):
hytte-plugin-proto → GTK-free wire protocol (node vocab, manifest, MessagePack framing, socket_path); language-neutral schema anchor, tokio optional
hytte-preem        → GTK-free leaf: the retro raster kit (#356) — dot_matrix, marquee, seven_seg, textbox, led_strip, scope, gauge, split_flap, font, Frame, DisplayStyle. Pure `std` plus one `hytte-plugin-proto` dep for `Frame::into_node`. Lived inside `hytte-plugin` until #859; extracted for the `hytte-config` reason — the shell wants to rasterise with it too (#857) and should not have to link the plugin *client* SDK to do it. `hytte-plugin` re-exports it (`pub use hytte_preem as preem;`), so every `hytte_plugin::preem::…` path a plugin already wrote still resolves
hytte-plugin       → the Rust plugin runtime SDK over the proto: TEA `Plugin` trait + `run()` (dial/backoff, Register handshake, session loop, render dedup). A plugin binary deps THIS crate alone
hytte-plugin-clock-demo → the reference plugin: pure manifest/init/update/view + one-line main
hytte-plugin-pet   → the kaomoji cat (#276): clock-driven moods, pokeable, optional llama-server brain (thin ureq client; canned fallback)
hytte-plugin-agents → hyperhive agents as sidebar pills (#947 P1; spec `docs/superpowers/specs/2026-09-07-agentic-desktop-design.md`). Notable for two things. First, it speaks hyperhive's `host.sock` through a **typed mirror it owns** (`src/hive/wire.rs`) rather than linking a hive crate — the workspace has no git dependencies (#757) and hyperhive publishes no client SDK yet (#948) — so the mirror carries only what the rows render, tolerates unknown keys, and **refuses** a wire `version` newer than its own instead of guessing (hivectl warns and continues there because a human reads its stderr; nobody reads a sidebar row's). Second, it is the first production consumer of `hytte-config`'s `Subsystem` (#868) — `agents.toml` — which is also the first time a *plugin* links `hytte-config`; the argument is #640's, a GTK-free leaf library rather than a runtime link. Its `Scope` type carries only `agent_names` and has no `Default`, because hyperhive's `LifecycleScope` reads an all-false scope as **everything**: the footgun is made unrepresentable rather than merely avoided. It declares `OpenPage` + `Notify` + `OpenUri` (#1045 — the narrow "name a destination" effect) and, since #950, `RunCommand` — the detached launch of the companion window (`src/window.rs`), narrowed not by the capability (which is arbitrary argv as the user, the highest-trust one in the vocabulary) but by the one `argv` this plugin can build: a constant binary name plus a re-parsed `AgentName`. `Consent` arrived with **P3, approvals** (#947 §6.5), withheld through P1 and P2 so the row was trustworthy before it could raise a modal that merges a config change: the hive's `Pending` queue rides the *existing* status poll (one tick, so the badge and the row it decorates are one observation, and the `SlotVisible` park/reload/seed logic exists once), a newly-queued approval raises `Effect::RequestConsent` with the new two-button card, and the answer goes back as exactly one `Approve { id }` or `Deny { id }` with **no standing grant persisted** — achieved by having nowhere to persist one. Three rules carry it: it prompts **once** per approval (a `prompted` set, pruned to the live queue); **silence decides nothing** (a timeout, `Esc`, or no output to draw on sends the hive nothing, the row keeps a badge counting what it waits on, and clicking the badge re-raises the oldest — deny is only ever a click); and a decision for an approval that left `Pending` is dropped with a debug line. The in-flight prompt is an `Option`, not a map, because the host has exactly one consent window, and it doubles as the gate that answers a burst one card at a time. `ApprovalKind`/`ApprovalStatus` are the mirror's first enum-typed fields and each carries a `#[serde(untagged)] Unknown(String)` arm, so a kind the hive grows costs that row its wording rather than blanking every badge on the card. The **card shape is Annika's v1, settled on the #963 thread rather than in the spec file**: one pill per agent, `[icon] [Name] [Model]` + `[start|stop] [edit]` over the harness status line, no chevron and no in-card details — and **both** of its destinations are #950's per-agent companion window (Annika on #947, 2026-09-11): the agent-page link opens it, the edit button opens it on its **settings tab**, and each keeps its P1 route as the fallback (the browser; this plugin's own page) when `trollshell-agent-window` is not on `PATH`. That fallback is chosen **before** the effect is emitted, by resolving the binary, because a detached launch cannot report a missing program — `systemd-run` answers `ok: true` as soon as the user manager takes the start job and the unit fails at exec where nobody is listening. Since #1282 item 2 the **pill itself** is a `Node::Button` (`row:<name>`) taking that same route — one `open_agent_terminal`, not a second copy of the window/browser decision — so the row click and the panel's link cannot drift, and the pen's `--tab settings` is untouched; the row's own controls stay their own click targets because GTK gives the inner button the gesture. #1010 routes that in-shell page to the centered dialog rather than the drawer, since this card is `Mount::Sidebar*` — with no change in this crate; what #1010 does not govern is the #950 `RunCommand` route, because a separate GTK window is not a page. Tested end to end against a **fake `host.sock`** in `tests/`, which is what let it ship before a hive existed on the laptop
trollshell-agent-window → the per-agent **companion window** (#950, phase P2 of #947): a separate windowed GTK4/libadwaita binary on the `trollshell-control-center` precedent, never linked into the shell, launched out of process by the agents plugin as `trollshell-agent-window --agent <name> [--tab settings]`. **Our chrome, their page**: a `WebKitGTK` view (the `webkit6` crate — the *only* thing in the tree that links a web engine) of hyperhive's own per-agent page with `?hide=header` appended (the composer stays since #1282 — the window is a place to talk to the agent, not only to watch it), surrounded by a header reading `host.sock` **directly** (icon, name, short model word, live status), start/stop/pause, and a settings tab. It never touches the embedded page's DOM — Mara's constraint on #947, since that page is slated for a swarm-level rewrite, so the whole contract with hyperhive's frontend is that one query parameter. It links `hytte-plugin-agents` as a **library** (`hive`, `model`, `config` are all `pub`) rather than re-mirroring the wire: two readers of one socket must agree byte for byte about its verbs. One **application id per agent** (`mov.vibec0re.trollshell.AgentWindow.<agent>`), so GApplication's own single-instance machinery is "one window per agent", with `HANDLES_COMMAND_LINE` forwarding a second launch's `--tab` instead of dropping it; a niri rule for all of them matches the prefix. The embedded view navigates to **exactly one origin** — the hive the window was opened for (`page::navigable_in_place`, enforced in `decide-policy`, everything else handed to the desktop's default handler and cancelled): the page is an agent's turn stream, i.e. content the agent's inputs can influence, and this window has no address bar to contradict a header that says "agent X". TLS: `HiveUrls` carries no CA (the ask is open on #948), and `webkit6` 0.6 has no `GTlsDatabase` seam — its only trust knobs are the session-wide errors policy and `allow_tls_certificate_for_host`, which pins **one certificate** compared against the presented leaf. So since #1234 the window does in two calls what cannot be done in one: `src/verify.rs` decides one of four routes per launch (`TROLLSHELL_AGENT_WINDOW_CERT` → pin that leaf; else `TROLLSHELL_AGENT_WINDOW_CA` or `<dir>/trust-bundle.pem` → open a bounded GIO TLS connection to the gateway, verify the chain it presents against those anchors with a `GTlsFileDatabase`, and pin the leaf it accepted; else `<dir>/gateway.pem` — hyperhive's own `cat leaf-only ca.pem`, so its first block **is** the leaf — → pin it with no probe; else the system store), where `<dir>` is `TROLLSHELL_AGENT_WINDOW_TLS_DIR` (rendered by both nix modules from `services.hyperhive…tls.stateDir`, so a same-host deploy needs no setting — Mara's ask on #1224) and `/var/lib/hive-tls` otherwise. The session's TLS-error policy is still `Fail` on every arm, never a global ignore; nothing touches the machine's trust store; and a failure renders an inline state naming the file this launch tried, what happened to it, and every route. That whole resolve runs **off the GTK main thread** since #1246 — `window.rs`'s `begin_probe` paints a "verifying the hive's certificate…" state (the same `AdwStatusPage` slot the failure card uses) and runs it on a `std::thread`, with the answer crossing back over a `tokio::sync::oneshot` the main context awaits, so a slow, stalling or unroutable hive costs a spinner and not a frozen window; `verify::PROBE_DEADLINE` (8 s) moved with it and now bounds how long the card can say "verifying", and a `probe: RefCell<Option<String>>` slot — #963's consent shape — is what stops the 2 s poll (and a second `--tab` activation) from starting a probe behind the one already running. The **machine-wide** route in that card and in `nix/module-common.nix` is `cp /var/lib/hive-tls/trust-bundle.pem ./hive-ca.pem` + `security.pki.certificateFiles = [ ./hive-ca.pem ];` — never the runtime path interpolated into that option, which #1234 found cannot build (`cacert`'s derivation opens it inside the sandbox) and, with the sandbox off, pins bytes it never hashed. `webkitgtk_6_0` is deliberately **not** in the list the shell's and the control-center's `wrapGAppsHook4` wraps read (`nix/package.nix`'s `webInputs` split, #1130 M1): it would land in their `GI_TYPELIB_PATH` and put 167 MiB of web engine in the runtime closure of two binaries that never load one — `checks.shell-has-no-web-engine` asserts that against the real closure
hytte-plugin-stats → system stats as a plugin (#1250, P1 of epic #1248; Discussion #1235). Notable for being the first binary in the tree **designed to run twice in one session**: a bar instance and a right-sidebar one, differing only in `HYTTE_PLUGIN_MOUNT` (#1159) and the `HYTTE_PLUGIN_ID` that #1250 added to the SDK beside it — the host allows one live connection per plugin id and drops a duplicate (`plugins/session.rs`'s `IdGuard`), so a second launch needs a second id and the launcher already knows it (the `programs.trollshell.plugins.<id>` attribute name). One `stats.toml` drives both: `[bar]` and `[sidebar]`, and an instance picks its table by the **family** of its effective mount (`Mount::is_bar`), so there is no per-instance flag and no second file. It is what made the SDK grow a reader for that. `run` still never *pushes* a plugin its own override — right for *placement* — but the *schema* question that left unanswered is now answered by `hytte_plugin::effective_mount`/`effective_mount_from` (#1317, graduated out of this crate's `mount` module and `hytte-claude-bridge`'s hand copy once a second plugin had copied the same six lines), so `settings_from` asks the SDK for the resolved mount instead of parsing the variable a second time. The resolver falls back to the manifest where `run` refuses, and that arm stays unreachable in a live process for the reason it always was: `run` parses the same value and exits before the first dial, and this plugin reaches the resolver only from `init`/`sources`, i.e. after that. It samples with `hytte-sensors` (the P0 leaf, #1249 — the shell's own samplers, so the card and the native Stats page cannot disagree) in its own process, under `spawn_blocking` because `read_gpu_with_cache` spawns `nvidia-smi` on an Nvidia box and the SDK's session runs on a current-thread runtime, behind `hytte_plugin::poll::Gate` so a closed sidebar samples nothing at all. The card is all `Node::Preem`: the per-core row as a `DotMatrix`, the package temperature as a `SevenSeg`, GPU load as a `Gauge`, load history as a `Scope`. The per-core row is the one real translation — `hytte_preem::LedMatrix`, what the native BlinkenLichten panel draws (#857), is **not on the wire** (that is #1156), so a core becomes one glyph *cell* from a five-step ramp that `card.rs`'s test proves monotone by counting set bits in the kit's own 5×7 font; a core's intensity is quantised rather than continuous, and past ~16 cores the lamps wrap into banks, which is what makes it read as a grid. Since P2 (#1251) the same binary also renders the **bar** instance — four chips keeping the shell's own `ts-cpu`/`ts-memory`/`ts-disk`/`ts-gpu` classes, each a `Node::Button` whose click emits `Effect::OpenPage(Page::PluginSelf)` (the manifest's one capability) to open the plugin's **own** drawer page: the native Stats page's CPU / Memory / GPU / Disks cards in the wire vocabulary, sparklines as `Scope`s, memory and swap as `LedStrip`s, the per-core row at the page's own width. Four chips and four cards, not five: the native `ts-services` chip and card count failed systemd units (a system-bus client) and flapping shell tasks (`hytte_reactive::health`, in-process state), neither reachable from a plugin — which is what the epic's P3 already had staying native. The `[bar]` table grew `memory`/`disk` beside the five P1 keys and **no key is read by only one surface**: `per_core` on a bar makes the CPU chip's lamp a per-core row, `memory`/`disk` on a sidebar card add a row apiece. The bar instance is also the one that **cannot park**: `poll::Gate` starts closed, so it seeds its own `SetVisible(true)` at `init` and forwards no host visibility push, rather than depending on the host's constant `visible: true` seed for bar mounts
…and 8 more plugin binaries (hytte-plugin-{audio-widget,bar-clock-demo,preem-demo,timer,terminal,caw,departures,weather}) following the same shape, plus hytte-plugin-infobroker and hytte-plugin-niri-layouts — the count drifts, so trust `ls crates/hytte-plugin-*` over this line. Those last two are the workspace's only bundled plugins with a standalone-CLI hat (`hytte-infobroker auth`/`get`/`grants`, #562; `hytte-plugin-niri-layouts apply <layout>`, #1019), and since #1116 both — plus `trollshell` itself, for its lone `--scan-aps` flag — parse their argv with `clap` instead of by hand, each gaining a hidden `completions <shell>` subcommand that nix's `installShellCompletion` invokes at build time (`nix/plugin.nix`, `nix/package.nix`) to ship bash/zsh/fish completions.
```

Shell code uses `use hytte::prelude::*;` (App, Bar, Edge, Monitor, bind*, Service, …) plus `hytte::gtk` / `hytte::adw` / `hytte::services::*`. Don't add direct deps on gtk/adw/futures-signals in the binary — go through the re-exports.

### `hytte-bus`

All D-Bus goes through here, never raw zbus. Connections are pooled singletons (lazy session + system), with reconnection handled in `connection.rs`. Builders: `call()`, `property()`, `proxy()`, `signals()`, `own_name()`, `export_object()` — each takes the target bus as an explicit first argument, e.g. `call(BusKind::System, "org.freedesktop.UPower")` (#447 retired the asymmetric per-builder defaults and the `.bus(…)` override; the bus is stated at the constructor so a call to a system daemon can never silently land on the session bus). Property/proxy subscriptions surface a `PropState`/`ProxyState` (`Loading` → `Loaded`/`Stale`) rather than blocking. zbus is still a direct dep of `hytte-services` only for `zvariant` data types and the `#[zbus::interface]` macros — not for constructing connections.

## The `trollshell` binary

`main.rs` builds the `App`, registering each service module with its own `.with(foo::service())` call (one line per service — see the `hytte::services::{…}` import list atop `main.rs` for the current roster; there's no count maintained here on purpose, it only rots), then in the body closure builds a `Bar` per monitor and installs overlays. **Multi-monitor is explicit**: iterate `app.monitors()` and react to `app.monitors_changed()` to rebuild bars on hot-plug (there is intentionally no `on_all_monitors` helper).

Source layout (each module has a consistent shape — match it when adding):

- `widgets/` — bar chips. Each `pub fn widget(monitor) -> gtk::Widget`, binds to service signals, and on click calls `modal::toggle(monitor, Page::…, &btn)`.
- `panels/` — drawer pages mounted into `modal.rs`'s per-monitor `gtk::Stack`. Each `pub fn panel_<name>() -> gtk::Widget`.
- `overlays/` — per-monitor layer-shell overlays (consent, dialog, frame, notifications, osd, prompt, sidebar). Each `pub fn install(…)` wires the overlay to a signal source. Two of them are not signal-driven and keep a connector→`Monitor` map for **one** window raised on the focused output instead: `consent` (#487) and, since #1010, `dialog` — the centered card a **sidebar**-mounted plugin's `Effect::OpenPage(Page::PluginSelf)` opens its own page in, where a **bar** chip's keeps opening the drawer (`plugins::effects::page_surface` is the whole rule, total over `Mount`). It is the drawer's own fullscreen-surface-plus-transparent-catcher shape at `Layer::Overlay` + `KeyboardMode::Exclusive`, so there is no dimming and no backdrop (Annika, #1010); the body is `plugins::plugin_dialog_slot()`, which is `plugin_panel_slot`'s builder over the dialog's **own** selection (`dialog_panel_id`) — the drawer's three `set_active_panel` sites and the dialog's `set_dialog_panel` never touch each other's handle, and `pump.rs`'s two "is a panel on screen" readers take the **union**.
- `modal.rs` — the slide-out drawer system (`Page` enum, per-monitor drawer window/revealer).
- `components/` — cross-cutting `pub(crate)` building blocks reused across panels.
- `assets.rs` — resolves bundled asset paths via `TROLLSHELL_DATA_DIR` (runtime env → compile-time env baked by Nix → `CARGO_MANIFEST_DIR` dev fallback). Asset sources live in the top-level `assets/` dir mirroring the runtime `share/` layout: `assets/trollshell/{style.css,icons/}` and `assets/hytte-ui/style.css`.
- `commands.rs` — `gio::ActionEntry`s registered on the `adw::Application` (`org.gtk.Actions`) so niri keybinds can drive drawer/power-menu/sidebar actions that are otherwise mouse-only (#219).
- `control.rs` — the `mov.vibec0re.trollshell.Control` D-Bus endpoint that `trollshell-control-center` (and future tabs) bind to; transport only, no UI (#390).
- `plugins/` — the out-of-process widget-plugin **host transport** (a module dir since #443, not a single file — `mod.rs` plus `listener.rs`/`session.rs`/`effects.rs`/`pump.rs`/etc.): the per-user socket listener, the GTK-side clock pump, and the effect broker (#35). Since #893 stage B it also holds `preem_gl/` — **the** renderer behind `preem_render` since #1157, and the GPU arm of it before that: one shader pipeline per kit kind (`program.rs`'s `Scope`; since #1143 `gauge.rs`'s `Gauge`, whose grid is the **native** buffer rather than the pre-upscale one, which is what fixes #1090's smeared needle — and which, on its own, only helped while that grid was what the screen showed: #1090's second report was a big dial stair-stepping, and the cause was the blit point-sampling the grid into a larger allocation, i.e. replicating it (measured: 100.0 % flat blocks and `max |Δ| 0` against the kit), so the blit now takes `dot_matrix.frag`'s `u_viewport != u_grid` branch and resolves the face, the lit layer and the halo at the fragment's own position; since #1144 `dot_matrix.rs`'s `DotMatrix`, which has no upscale at all — the dot pitch is its size knob, #1091 — and whose improvement is instead that the dot lattice is evaluated at the _fragment's_ position rather than replicated out of the kit's fixed table, so a chip a layout scales above its natural size draws round dots at the screen's resolution), their `*.vert`/`*.frag` (under `src/`, `include_str!`'d, with a matching clause in `nix/package.nix`'s crane filter — the filter is by extension, so `src/` alone does not save them, and the `glsl` flake check compiles them, splice by splice), the pure state→uniform mappings, and the two fallback latches. **GL is the only arm**: #1157 retired the CPU renderer and the `TROLLSHELL_PREEM_RENDERER=cpu` kill switch once every kind had a shader (Annika on #865, "CPU renderer gone soon? ❤️"), so `preem_gl::Arm` no longer answers _which renderer_ but _whether this pipeline can draw_, and the other answer is the broken-widget placeholder — the degradation `Node::Shader` had taken alone by design since #893. Two things take a widget there, both sticky for the session and both folded in by `arm_for`: a failed `GdkGLContext`, and a pipeline this driver refused to build (#1232, per program, so a refused `preem.scope` leaves `preem.gauge` on the GPU). The kinds themselves are `Scope`, `Gauge`, `DotMatrix`, `Marquee`, `TextBox`, `LedStrip`, `SevenSeg`, `FlipBoard` and `LedMatrix` — #1152 took the two text kinds on Annika's word for the rest of #865, and the `Marquee` needed no shader of its own: a ticker is the same dot hardware on a continuous grid, so it registers `dot_matrix`'s pipeline under its own name and differs in three `site_at` uniforms, while the `TextBox` is one pass with no aux textures whose improvement is the rounded corner drawn as a distance at the fragment's own resolution, and #1153's `LedStrip` is smaller still — one pass, no aux textures and **no inputs at all**, because the kit's bloom over a union of axis-aligned rectangles has a closed form, so the halo is _computed_ at the fragment rather than box-blurred into a grid-resolution texture and read back; that is also the one arm whose bit-exactness is asserted hermetically, by a Rust transcription of the shader held to the shipped GLSL by a source scan and to the kit by its own bytes. Its title says "round LEDs"; the kit's segments are 8×16 bars and the same issue asks for a bit-exact 1:1 pin against that kit, so the shape stayed the kit's and making it round is a `hytte-preem` change still open on the thread; and #1156's `LedMatrix` — the Stats drawer's per-core panel, the one kind on this seam that is **not on the wire**, so `panels/stats.rs` builds its surface directly and `Kind::on_the_wire` is what keeps the enumeration tests honest about it — is that same closed-form pipeline in two dimensions, with the per-lamp brightness and the per-lamp ink the kit's `lamp_intensities`/`lamp_inks` resolve arriving as four floats per slot of the data strip. `hytte-preem` **stays** and does three jobs: it is the parity oracle the harness measures each shader against, the rasteriser every _plugin's_ own `Frame::into_node` runs in the plugin's process (the wire contract is untouched by #1157), and where the shell reads its widgets' geometry and palette from — it is the _source_ of the gauge arm's geometry rather than a thing the shell transcribes, since #1148's review made `Gauge::dial` and the shape constants `pub` in `hytte-preem` (additive visibility, zero behaviour change) and deleted the shell's hand mirror; #1144 follows it with `DotCell`/`dot_cell`. `preem_gl/parity.rs`'s `Kind` and `Sampling` are where "which comparisons `TROLLSHELL_PARITY_EXACT=1` pins bit-exact" is written down: **every** kind where the shader and the kit rasterise at the same resolution (all nine have measured `max |Δ| 0` under llvmpipe — and #1155's `FlipBoard` is the first whose 1:1 branch is not a point test, since the kit itself area-averages the falling card there, so its pin is a statement about operation order and about the 2.5 ULP GLSL ES allows the two divisions `FlipBoard::compose_flap` spells), and none where the GL frame was rendered at a higher resolution and box-averaged back down — those supersampled cases assert instead that every pixel off a rasterisation edge is bit-identical **and** that the edge region stays inside a per-kind budget calibrated from measurement, which is the only standard a supersampled comparison can meet and a sharper one than the #893 ceiling. That variable is exported nowhere but `nix/checks/system-tests.nix`'s sandbox, so a pin there is a CI regression detector against one known driver — what a real driver answers to is the #893 on-glass ceiling (mean 2 / p99 8 / max 32 per channel), which is the half Annika's "does not have to be pixel perfect identical" (#865) governs. Since #893 it also holds `shader_map.rs` — the host half of the **shader widget** (`Node::Shader`: a plugin ships a fragment body plus a data buffer, the shell compiles the body once in `hytte-ui`'s `ShaderSurface` and per frame re-uploads only the buffer). Its trust boundary is **route 0, the socket itself** (spec §"Trust boundary for #893"): `Capability::Shader` is an ordinary auto-granted manifest capability, the enforced checks are a 16 KiB source cap / a 4 MiB data cap / the buffer-shape invariant / the capability, each degrading to the broken-widget placeholder with one warning — and there is deliberately **no runtime validator**, because naga cannot parse GLSL ES at all. Blast radius of a runaway shader is the whole shell; GL-only by design, like every preem widget since #1157.
- `plugin_launcher.rs` — the declarative plugin **launcher** (#419): reads the nix-written `trollshell/plugins.json` (XDG config, rendered from `programs.trollshell.plugins`) and launches each enabled plugin as a transient `trollshell-plugin-<id>` user unit via `systemd-run --user`; the control-center Plugins tab's start/stop (#348) routes through it, and its `extra_env` spawn hook is where #392's key injection rides. Hand-installed static units under `etc/` keep working (legacy fallback).
- `revision.rs` — resolves the build's git revision via `TROLLSHELL_REV` (runtime env → compile-time env → `"dev"` fallback), the same three-tier shape as `assets.rs` (#601). The nix side injects it **only** through the cheap wrapper slices' `preFixup` (`nix/package.nix`, `nix/control-center.nix`) — never as a compile-time env, which would rehash the single `workspace` crane compile on every commit. Surfaced over D-Bus as `Control.Revision`; whether it also gets a UI surface is still open.
- `scale.rs` — font-relative pixel scaling (`scale()`) for the handful of Rust-set sizes CSS `em` can't reach (#135).

### Conventions

- CSS classes: `hytte-*` come from the library default stylesheet (`assets/hytte-ui/style.css`); `ts-*` come from the binary's `assets/trollshell/style.css` (loaded as user style at higher priority).
- App ID `mov.vibec0re.trollshell`; D-Bus agent names `mov.vibec0re.trollshell.{bluez,iwd}-agent` (polkit is now a standalone external agent, not in-process). The NetworkManager secret agent (`wifi/nm_agent.rs`) registers with NM's `AgentManager` (no extra bus-name; `RegisterWithCapabilities`) rather than owning a `mov.vibec0re.*` name.
- Logging via `tracing`.

## Deployment & session integration

`etc/` holds the full Niri-session config the shell expects (systemd user units incl. `trollshell.service` and `niri-session.target`, niri keybinds, kanshi display profiles, cliphist) — see `etc/README.md`. The idle → dim → lock → suspend pipeline is **native** (in-process; `crates/hytte-services/src/idle_notify.rs`, an `ext-idle-notify-v1` client gated on logind inhibitors — #204 retired swayidle), so there is no idle daemon unit. trollshell ships no in-shell lock screen; locking is delegated to `swaylock`, driven by the native idle timer / logind's `Lock` signal — see `etc/README.md`'s "Idle & screen locking" section for wiring `swaylock`'s own PAM stack. The flake exposes `nixosModules.default` (`programs.trollshell.enable`), which installs the package. Everything else it pulls in — the system-bus policy permitting the two agent names (`bluez`/`iwd`), the polkit-gnome user service, UPower, power-profiles-daemon, and geoclue2 — sits behind `programs.trollshell.enableRecommendedServices` (default `true`); each is `mkDefault` so an explicit `enable = false;` still wins. With the switch off, the chips that back onto a missing daemon hide themselves (battery on `BatteryState::Unknown`, the power-profile group on empty `available`, weather falls back to `TROLLSHELL_WEATHER_CITY`) and the bluez/iwd agents park inert on `AccessDenied`. A `homeModules.default` (home-manager) runs the shell as a user service and shares the same option base (`nix/module-common.nix`).

## Known gotchas

- **Niri fullscreen detection:** `WindowLayoutsChanged` is the _only_ niri-ipc event that fires on a fullscreen toggle (`WindowsChanged`/`WindowOpenedOrChanged` do not). The frame overlay relies on this.
- **Icons render as `image-missing`** if you run outside the devShell, or if the icon theme isn't forced — `main.rs` calls `set_gtk_icon_theme_name("Adwaita")` to work around GSettings schemas not being visible under `cargo run`.
