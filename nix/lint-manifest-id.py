#!/usr/bin/env python3
"""Fail if the nix-side `inferManifestId` and its Rust hand-mirror disagree.

THE DEFECT
----------
`crates/trollshell-control-center/src/plugins_tab.rs`'s `manifest_id_of_exec`
is a hand transcription of `nix/module-common.nix`'s `inferManifestId` — the
rule that turns a plugin's `exec` basename into the id it registers under
(strip a `hytte-plugin-` prefix; failing that, strip a bare `hytte-` prefix;
failing that, keep the name as-is). The control-center reads a plugin's OWN
manifest id off this function to decide which `config_form::FamilyOps` (if
any) that plugin's settings form uses — so if the two rules ever disagree,
the control-center looks up the wrong family for a plugin (or none at all,
which is a silently legitimate answer: a settings form just stops mounting,
with no error).

Before this script, nothing checked the two agreed — a change to either side
shipped green. From the #1365 adversarial review (L5), filed as #1372.

WHAT IT CHECKS
--------------
`nix/manifest-id-cases.txt` is a table of `exec<TAB>expected-manifest-id`
rows, generated once by evaluating the REAL nix function (see that file's own
header for the exact commands and why it is a `nix-build`-driven derivation
rather than a bare `nix eval`/`nix-instantiate --eval`). This script:

  1. Extracts `inferManifestId`'s two prefix STRING LITERALS
     (`"hytte-plugin-"`, `"hytte-"`) out of `nix/module-common.nix`'s actual
     source, by anchoring on the whole known shape of the `let … in …`
     expression that defines it. If that shape has changed at all — not just
     the literals, the control flow around them — the anchor fails to match
     and the scan refuses to guess (exit 2), rather than silently checking
     against stale or partially-extracted literals.
  2. Reimplements the RULE (not just the literals) in Python:
     `baseNameOf` → basename-of-the-last-path-segment, then try the plugin
     prefix, falling back to the bare prefix only when the plugin prefix did
     not actually strip anything — mirroring `if afterPluginPrefix != binName
     then … else …` exactly, not "if binName starts with the plugin prefix"
     (a distinction that matters for the degenerate empty-suffix case, i.e.
     `exec = "hytte-plugin-"`, though the checked-in table currently doesn't
     carry that literal row — see its own header for why the empty-string
     case was dropped from the file).
  3. Runs that reimplementation, with the literals from step 1, over every
     row of the table and reports every mismatch.

Because step 1 re-reads the literals from the tree on every invocation rather
than hardcoding `"hytte-plugin-"`/`"hytte-"` as Python constants, a nix-side
edit that changes *only* the prefix spelling (not the shape around it) is
caught as a real per-row divergence against the table's frozen answers —
`self_test()` below proves this by re-running the reimplementation with a
deliberately wrong prefix and checking the answer changes. A structural
change (a different algorithm entirely) fails at the anchor instead, which is
the same "cannot find my anchor, so I don't trust my own answer" posture
`nix/lint-config-vocab.py` and `nix/lint-bind-pins.py` already use.

THE RUST SIDE IS NOT RUN HERE
------------------------------
`crates/trollshell-control-center/src/plugins_tab.rs`'s own
`manifest_id_of_exec_matches_the_shared_table` test (`mod tests`) reads the
SAME `nix/manifest-id-cases.txt` and checks `manifest_id_of_exec` against it.
That test runs under `cargo test`/`checks.workspace-tests`; this script does
not compile or invoke Rust at all — it stays a `pkgs.runCommand` with no
`cargoArtifacts`, on the `bind-pins`/`config-vocab`/`lints-tables` posture:
no compile, red in seconds. The table is what ties the two runs together —
one file two independent checks both grade against — rather than either
check reaching across into the other's language.

WHY A HAND-ROLLED SCAN AND NOT A NIX-EVAL ROUND-TRIP
-----------------------------------------------------
`inferManifestId` happens to be a leaf function — it closes over only its own
`pkg` argument and the module's `lib`, nothing else this module or any other
module contributes — so unlike the option-declaration literals
`nix/lint-config-vocab.py`'s header discusses, it COULD be read for real
without a full `evalModules` pass, by directly importing
`nix/module-common.nix` and applying it to a minimal `{ config = { }; lib;
pkgs; }`. `nix/manifest-id-cases.txt`'s own header does exactly that, ONCE,
to generate the table's expected column. Doing it here, on every scan run,
would mean this `pkgs.runCommand` shelling out to `nix build`/`nix eval`
against `${self}` from inside its own sandboxed build — recursive Nix, which
needs its own opt-in (`nix.settings.extra-experimental-features =
"recursive-nix"` at the *daemon*, not just this flake) and is not enabled
here, so the derivation would have no `nix` binary, no store access beyond
its own inputs, and no network. A hand-rolled scan needs none of that and
reds in seconds, on the same posture `nix/lint-config-vocab.py`'s "WHY A
HAND-ROLLED SCAN AND NOT A NIX-EVAL ROUND-TRIP" section argues at more
length for the option-declaration case.

Run it by hand from the repo root with:

    nix shell nixpkgs#python3 --command python3 nix/lint-manifest-id.py

The `nix shell` is not optional: **`python3` is deliberately not on the
devShell PATH** (see `nix/lint-bind-pins.py`'s own header), so a bare
`python3 nix/lint-manifest-id.py` is `command not found`.

USAGE
-----
    python3 nix/lint-manifest-id.py

Exits 0 when every row agrees, 1 naming every mismatch, 2 when the scan
itself is untrustworthy (a source file is missing, the rule's anchor cannot
be found, the table is malformed or empty, or `self_test()` — run first, on
every invocation — disagrees with its own fixtures).
"""

import os
import re
import sys
import tempfile
import traceback

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
MODULE_COMMON_NIX = os.path.join(REPO_ROOT, "nix", "module-common.nix")
CASES_FILE = os.path.join(REPO_ROOT, "nix", "manifest-id-cases.txt")

# Anchors the WHOLE known shape of `inferManifestId`'s definition, not just
# the two literals — see "WHAT IT CHECKS" step 1 above for why a structural
# change must fail here (exit 2) rather than silently extract nothing useful.
RULE_PATTERN = re.compile(
    r"config\._module\.args\.inferManifestId\s*=\s*"
    r"pkg:\s*"
    r"let\s*"
    r"binName\s*=\s*baseNameOf\s*\(lib\.getExe\s+pkg\);\s*"
    r'afterPluginPrefix\s*=\s*lib\.removePrefix\s*"([^"]*)"\s*binName;\s*'
    r"in\s*"
    r"if\s+afterPluginPrefix\s*!=\s*binName\s+then\s+afterPluginPrefix\s+else\s+"
    r'lib\.removePrefix\s*"([^"]*)"\s*binName;'
)


def remove_prefix(prefix: str, s: str) -> str:
    """`lib.removePrefix prefix s`: strip `prefix` only when `s` starts with
    it at position 0; otherwise return `s` unchanged (never a substring
    search)."""
    return s[len(prefix) :] if s.startswith(prefix) else s


def extract_prefixes(nix_src: str) -> tuple[str, str]:
    """The two string literals `inferManifestId` strips, read off the real
    source text — see RULE_PATTERN's comment for why the match is anchored
    to the whole shape rather than just the two `lib.removePrefix "…"`
    calls."""
    m = RULE_PATTERN.search(nix_src)
    if not m:
        raise LookupError(
            "inferManifestId's definition was not found in nix/module-common.nix "
            "in the expected shape (`binName = baseNameOf (lib.getExe pkg); "
            'afterPluginPrefix = lib.removePrefix "…" binName; … if '
            "afterPluginPrefix != binName then afterPluginPrefix else "
            'lib.removePrefix "…" binName;`) — either the rule was reshaped '
            "(update RULE_PATTERN in nix/lint-manifest-id.py to match) or the "
            "anchor drifted"
        )
    return m.group(1), m.group(2)


def infer_manifest_id(exec_str: str, plugin_prefix: str, bare_prefix: str) -> str:
    """A faithful transcription of `inferManifestId`'s BODY (not its
    literals) — `baseNameOf`, then the plugin prefix, falling back to the
    bare prefix only when the plugin prefix did not actually strip
    anything."""
    bin_name = exec_str.rsplit("/", 1)[-1]
    after_plugin_prefix = remove_prefix(plugin_prefix, bin_name)
    if after_plugin_prefix != bin_name:
        return after_plugin_prefix
    return remove_prefix(bare_prefix, bin_name)


def read_cases(path: str) -> list[tuple[str, str]]:
    """Parse `nix/manifest-id-cases.txt`: `exec<TAB>expected-id` rows, blank
    lines and `#`-comments ignored. `str.split("\\t")` rather than
    `.split()`/`.rstrip()`, so a legitimately-empty trailing field survives —
    trailing-whitespace trimming is exactly what silently ate the table's
    first draft of the degenerate empty-suffix rows (see that file's
    header)."""
    cases = []
    try:
        with open(path, encoding="utf-8") as fh:
            lines = fh.read().split("\n")
    except OSError as e:
        raise LookupError(f"cannot read {path}: {e}") from e
    for lineno, line in enumerate(lines, 1):
        if not line or line.startswith("#"):
            continue
        parts = line.split("\t")
        if len(parts) != 2:
            raise LookupError(
                f"{path}:{lineno}: expected `exec<TAB>expected-id`, got {line!r}"
            )
        cases.append((parts[0], parts[1]))
    if not cases:
        raise LookupError(f"{path}: no data rows found")
    return cases


# ── self-test ────────────────────────────────────────────────────────────────


def self_test() -> list[str]:
    failures = []

    fixture_src = """
  config._module.args.inferManifestId =
    pkg:
    let
      binName = baseNameOf (lib.getExe pkg);
      afterPluginPrefix = lib.removePrefix "hytte-plugin-" binName;
    in
    if afterPluginPrefix != binName then afterPluginPrefix else lib.removePrefix "hytte-" binName;
"""
    plugin_prefix, bare_prefix = extract_prefixes(fixture_src)
    if (plugin_prefix, bare_prefix) != ("hytte-plugin-", "hytte-"):
        failures.append(
            f"extract_prefixes: got {(plugin_prefix, bare_prefix)!r}, "
            "expected ('hytte-plugin-', 'hytte-')"
        )

    # The anchor must refuse text that does not have the expected shape,
    # rather than matching partially and returning nonsense.
    try:
        extract_prefixes("this is not the rule at all")
    except LookupError:
        pass
    else:
        failures.append("extract_prefixes matched source with no inferManifestId in it")

    def expect(exec_str: str, expected: str, plugin_pfx: str = plugin_prefix, bare_pfx: str = bare_prefix) -> None:
        got = infer_manifest_id(exec_str, plugin_pfx, bare_pfx)
        if got != expected:
            failures.append(f"infer_manifest_id({exec_str!r}): got {got!r}, expected {expected!r}")

    expect("hytte-plugin-stats", "stats")
    expect("/nix/store/xxx-hytte-plugin-stats/bin/hytte-plugin-stats", "stats")
    expect("hytte-claude-bridge", "claude-bridge")
    expect("trollshell-agent-window", "trollshell-agent-window")
    expect("hytte-plugin-x", "x")
    expect("hytte-x", "x")
    expect("not-hytte-plugin-foo", "not-hytte-plugin-foo")
    # The degenerate whole-name-is-the-prefix case, exercised here even
    # though the checked-in table doesn't carry it (see that file's header).
    expect("hytte-plugin-", "")
    expect("hytte-", "")

    # THE SENSITIVITY PROOF: re-deriving the literals from source (rather
    # than hardcoding them) must actually change the computed answer when
    # the literal changes — otherwise step 1 of "WHAT IT CHECKS" is theatre
    # and this scan cannot detect a mutated nix rule at all.
    mutated_plugin_prefix = "hytte-widget-"
    mutated = infer_manifest_id("hytte-plugin-stats", mutated_plugin_prefix, bare_prefix)
    if mutated == "stats":
        failures.append(
            "infer_manifest_id ignored the mutated plugin prefix — it must not "
            "hardcode 'hytte-plugin-' independently of what extract_prefixes found "
            f"(with prefix {mutated_plugin_prefix!r}, 'hytte-plugin-stats' no "
            "longer starts with it, so the bare 'hytte-' fallback should have "
            "fired, giving 'plugin-stats')"
        )
    elif mutated != "plugin-stats":
        failures.append(
            f"infer_manifest_id with a mutated plugin prefix gave {mutated!r}, "
            "expected 'plugin-stats' (falls through to the bare hytte- prefix)"
        )

    # Parsing rejects a row that isn't exactly two tab-separated fields, and
    # accepts blank lines / comments without producing a row for them.
    with tempfile.NamedTemporaryFile("w", suffix=".txt", delete=False, encoding="utf-8") as fh:
        fh.write("# a comment\n\nhytte-plugin-stats\tstats\nhytte-x\tx\n")
        good_path = fh.name
    try:
        rows = read_cases(good_path)
        if rows != [("hytte-plugin-stats", "stats"), ("hytte-x", "x")]:
            failures.append(f"read_cases: got {rows!r} for a well-formed table")
    finally:
        os.unlink(good_path)

    with tempfile.NamedTemporaryFile("w", suffix=".txt", delete=False, encoding="utf-8") as fh:
        fh.write("hytte-plugin-stats\tstats\textra-field\n")
        bad_path = fh.name
    try:
        try:
            read_cases(bad_path)
        except LookupError:
            pass
        else:
            failures.append("read_cases accepted a row with more than one tab")
    finally:
        os.unlink(bad_path)

    return failures


def _self_test_failed(lines: list[str]) -> int:
    """See `nix/lint-lints-tables.py`'s own `_self_test_failed` — same shape,
    same exit code (2), so a scanner bug and a real finding never read the
    same way in CI output."""
    print("manifest-id scan: SELF-TEST FAILED", file=sys.stderr)
    for line in lines:
        print(f"  {line}", file=sys.stderr)
    print(
        "\nThe scanner disagrees with its own fixtures, so any verdict it gives on\n"
        "the tree is meaningless. Fix the comparison, not the fixtures.",
        file=sys.stderr,
    )
    return 2


def main() -> int:
    try:
        failures = self_test()
    except Exception as e:  # noqa: BLE001 - see _self_test_failed's reasoning
        return _self_test_failed(
            [f"a fixture raised {type(e).__name__}: {e}"]
            + ["  " + line for line in traceback.format_exc().rstrip().splitlines()]
        )
    if failures:
        return _self_test_failed(failures)

    try:
        with open(MODULE_COMMON_NIX, encoding="utf-8") as fh:
            nix_src = fh.read()
    except OSError as e:
        print(f"manifest-id scan: cannot read {MODULE_COMMON_NIX}: {e}", file=sys.stderr)
        print("  (run from inside the repository)", file=sys.stderr)
        return 2

    try:
        plugin_prefix, bare_prefix = extract_prefixes(nix_src)
    except LookupError as e:
        print(f"manifest-id scan: {e}", file=sys.stderr)
        return 2

    try:
        cases = read_cases(CASES_FILE)
    except LookupError as e:
        print(f"manifest-id scan: {e}", file=sys.stderr)
        return 2

    problems = []
    for exec_str, expected in cases:
        got = infer_manifest_id(exec_str, plugin_prefix, bare_prefix)
        if got != expected:
            problems.append(
                f"{exec_str!r}: nix's inferManifestId (as read from "
                f"nix/module-common.nix) answers {got!r}, but "
                f"nix/manifest-id-cases.txt expects {expected!r}"
            )

    if problems:
        print(
            f"\nERROR: {len(problems)} manifest-id mismatch(es) against "
            "nix/manifest-id-cases.txt:\n",
            file=sys.stderr,
        )
        for line in problems:
            print(f"  - {line}", file=sys.stderr)
        print(
            "\nEither nix/module-common.nix's inferManifestId changed (update\n"
            "nix/manifest-id-cases.txt's expected column AND\n"
            "crates/trollshell-control-center/src/plugins_tab.rs's\n"
            "manifest_id_of_exec to match, in the same commit — see that file's\n"
            "own doc comment), or the nix rule regressed.",
            file=sys.stderr,
        )
        return 1

    print(
        f"manifest-id scan: {len(cases)} case(s) agree with nix's inferManifestId "
        f"(plugin prefix {plugin_prefix!r}, bare prefix {bare_prefix!r})",
        flush=True,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
