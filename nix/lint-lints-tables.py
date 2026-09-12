#!/usr/bin/env python3
"""Fail if an `unsafe` island's hand-mirrored `[lints]` tables drift from root.

THE DEFECT
----------
The workspace lint config lives in the root `Cargo.toml`'s
`[workspace.lints.rust]` / `[workspace.lints.clippy]`, and every member
inherits it with `[lints] workspace = true` — except the two `unsafe` islands,
`crates/hytte-ecal` (FFI to libecal) and `crates/hytte-gl` (OpenGL entry
points). Those two need `unsafe_code = "allow"` where root says `"forbid"`,
and Cargo's workspace-lints inheritance is **all-or-nothing**: there is no
per-lint override. So each island hand-copies the whole table with that one
line flipped, and all three tables carry a comment telling the next editor to
keep them in sync.

Nothing enforced that comment. Deleting `pedantic = { level = "deny", … }`
from `crates/hytte-gl/Cargo.toml` leaves `cargo build`, `cargo clippy
--workspace --all-targets -- -D warnings`, `cargo test` and the whole of
`nix flake check` green — the crate simply stops being pedantic-checked, and
the workspace's "code must be pedantic-clean" rule silently stops applying to
one of the two crates where `unsafe` is legal. Same for `disallowed_methods`
(the raw-zbus ban), and same in the other direction: root gaining a lint that
the islands never receive.

`#1179` (from the `#1162` sweep) is where that was measured; this script is
what makes "the three tables are in sync" a checkable fact rather than a
comment.

WHAT IT CHECKS
--------------
For each island (`ISLANDS` below):

  1. It does **not** use `[lints] workspace = true` (that would be a different
     shape entirely, and would not compile with its `unsafe_code` need).
  2. `[lints.rust]` and `[lints.clippy]` both exist.
  3. `unsafe_code` is `"allow"` — the one permitted divergence, and it must be
     spelled, not merely absent (an island that dropped the line would fail to
     compile, but it would fail confusingly).
  4. Every **other** key in root's two tables is present in the island's
     corresponding table with an **equal value** (values are compared as
     parsed TOML, so `{ level = "deny", priority = -1 }` compares as a whole,
     not as text).
  5. Every key the island adds beyond root's is one of that island's declared
     carve-outs in `EXTRA_ALLOWS` below, **with the value declared there**.

And for every other workspace member (read from root's `workspace.members`):

  6. It uses `[lints] workspace = true` and declares no `[lints.rust]` /
     `[lints.clippy]` of its own — so a *third* island cannot appear without
     this script (and a reviewer) seeing it.

THE CARVE-OUT (rule 5, do not regress this)
-------------------------------------------
`hytte-ecal` layers five FFI-only `allow`s on top of the mirrored table
(`missing_safety_doc`, `doc_markdown`, `must_use_candidate`, `ref_as_ptr`,
`borrow_as_ptr`), each justified by a comment in its `Cargo.toml`: wrapping a
C library trips those constantly and they say nothing about this code. Those
are legitimate and are declared in `EXTRA_ALLOWS`.

They are declared **here** rather than waved through as "anything extra is
fine" on purpose: an `allow` added to an island is a lint exemption that no
other crate in the workspace gets, and the point of this check is that such a
thing is visible. Adding a sixth means editing this file in the same commit,
which is exactly the review moment that would otherwise be missing.

WHY THIS IS A NIX LINT AND NOT A `cargo test`
----------------------------------------------
A `cargo test` *could* read `Cargo.toml` files (unlike the `.nix` files
`nix/lint-core-leds-vocab.py` and `nix/lint-bridge-socket.py` need — the crane
source filter keeps `.toml`). It still should not: the natural home for such
a test is one of the islands, which would make the crate that must not drift
the crate that certifies it hasn't, and the test would have to be duplicated
or given a home in a third crate that has no other reason to exist. It is
also a *source-level* defect that no compile in this flake can see, which is
precisely the `bind-pins` / `glsl` / `core-leds-vocab` / `bridge-socket`
posture: a `pkgs.runCommand` with no `cargoArtifacts`, red in seconds.

Run it by hand from the repo root with:

    nix shell nixpkgs#python3 --command python3 nix/lint-lints-tables.py

The `nix shell` is not optional: **`python3` is deliberately not on the
devShell PATH** (see `nix/lint-bind-pins.py`'s own header), so a bare
`python3 nix/lint-lints-tables.py` is `command not found`.

USAGE
-----
    python3 nix/lint-lints-tables.py

Exits 0 when all three tables agree, 1 naming every divergence found, 2 when
the scan itself is untrustworthy (a manifest is missing or unparseable, root
has no `[workspace.lints]`, or `self_test()` — run first, on every invocation
— disagrees with its own fixtures).
"""

import os
import sys
import tomllib

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ROOT_MANIFEST = os.path.join(REPO_ROOT, "Cargo.toml")

# The two crates allowed to hand-mirror the table (CLAUDE.md, "Lint — strict"):
# relative manifest directory → human name used in messages.
ISLANDS = {
    "crates/hytte-ecal": "hytte-ecal",
    "crates/hytte-gl": "hytte-gl",
}

# The lint tables that must match. `[lints.rust]` and `[lints.clippy]` are the
# only two Cargo defines today; a third would need adding here deliberately.
TABLES = ("rust", "clippy")

# The one key an island is *expected* to diverge on, and the value each side
# must spell.
UNSAFE_CODE = "unsafe_code"
ROOT_UNSAFE_CODE = "forbid"
ISLAND_UNSAFE_CODE = "allow"

# Rule 5: keys an island may carry beyond root's table, with the exact value
# each must have. Every entry here is a lint exemption no other crate in the
# workspace has — see "THE CARVE-OUT" above before adding one.
EXTRA_ALLOWS = {
    "hytte-ecal": {
        "clippy": {
            # Wrapping a C library: the safe wrappers in lib.rs are the
            # contract, not a `# Safety` section on every `pub unsafe fn`.
            "missing_safety_doc": "allow",
            # Every GObject/GLib type name reads as "missing backticks".
            "doc_markdown": "allow",
            # Most safe wrappers return values that ARE the API.
            "must_use_candidate": "allow",
            # `&mut err` for `*mut *mut GError` is idiomatic FFI.
            "ref_as_ptr": "allow",
            "borrow_as_ptr": "allow",
        },
    },
    "hytte-gl": {},
}


def read_manifest(path: str) -> dict:
    with open(path, "rb") as fh:
        return tomllib.load(fh)


def root_lint_tables(root: dict) -> dict[str, dict]:
    """Root's `[workspace.lints.rust]` / `[workspace.lints.clippy]`."""
    lints = root.get("workspace", {}).get("lints")
    if not isinstance(lints, dict):
        raise LookupError("root Cargo.toml has no [workspace.lints] section")
    out = {}
    for table in TABLES:
        got = lints.get(table)
        if not isinstance(got, dict):
            raise LookupError(f"root Cargo.toml has no [workspace.lints.{table}] table")
        out[table] = got
    return out


def crate_lint_tables(manifest: dict) -> dict:
    """A member's own `[lints]` section, as parsed (may be empty/absent)."""
    lints = manifest.get("lints")
    return lints if isinstance(lints, dict) else {}


def compare_island(
    name: str,
    root_tables: dict[str, dict],
    lints: dict,
    extras: dict,
) -> list[str]:
    """Every way `name`'s hand-mirrored tables differ from root's (rules 1-5)."""
    problems = []

    if lints.get("workspace") is True:
        problems.append(
            f"{name}: uses `[lints] workspace = true`, but it is an unsafe island — "
            "it must hand-mirror the root table with `unsafe_code = \"allow\"`"
        )
        return problems

    for table in TABLES:
        island = lints.get(table)
        if not isinstance(island, dict):
            problems.append(f"{name}: has no [lints.{table}] table at all")
            continue
        root = root_tables[table]

        # Rule 3: the one permitted divergence, on the table that carries it.
        if UNSAFE_CODE in root:
            got = island.get(UNSAFE_CODE)
            if got != ISLAND_UNSAFE_CODE:
                problems.append(
                    f"{name}: [lints.{table}] {UNSAFE_CODE} is {got!r}, "
                    f"expected {ISLAND_UNSAFE_CODE!r} (root says "
                    f"{root[UNSAFE_CODE]!r}; this is the *only* key an island "
                    "may differ on)"
                )

        # Rule 4: every other root key, present and equal.
        for key, value in sorted(root.items()):
            if key == UNSAFE_CODE:
                continue
            if key not in island:
                problems.append(
                    f"{name}: [lints.{table}] is missing {key!r} — root declares "
                    f"{key} = {value!r} and this crate does not inherit it"
                )
            elif island[key] != value:
                problems.append(
                    f"{name}: [lints.{table}] {key} = {island[key]!r}, "
                    f"but root says {value!r}"
                )

        # Rule 5: extras must be declared carve-outs, with the declared value.
        allowed = extras.get(table, {})
        for key, value in sorted(island.items()):
            if key in root:
                continue
            if key not in allowed:
                problems.append(
                    f"{name}: [lints.{table}] declares {key} = {value!r}, which root "
                    "does not have and nix/lint-lints-tables.py does not list as a "
                    f"carve-out for {name} — add it to EXTRA_ALLOWS (with the "
                    "reason) or drop it"
                )
            elif value != allowed[key]:
                problems.append(
                    f"{name}: [lints.{table}] {key} = {value!r}, but "
                    f"EXTRA_ALLOWS declares {allowed[key]!r} for it"
                )

    return problems


def check_non_island(name: str, lints: dict) -> list[str]:
    """Rule 6: a non-island member inherits, and hand-rolls nothing."""
    problems = []
    if lints.get("workspace") is not True:
        problems.append(
            f"{name}: does not use `[lints] workspace = true` — every member that "
            "is not an unsafe island inherits the root table"
        )
    for table in TABLES:
        if isinstance(lints.get(table), dict):
            problems.append(
                f"{name}: declares its own [lints.{table}] table — only the unsafe "
                f"islands ({', '.join(sorted(ISLANDS.values()))}) may do that, and a "
                "new one needs adding to nix/lint-lints-tables.py's ISLANDS"
            )
    return problems


# ── self-test ────────────────────────────────────────────────────────────────


def _root_fixture() -> dict[str, dict]:
    return {
        "rust": {"unsafe_code": "forbid"},
        "clippy": {
            "all": {"level": "deny", "priority": -1},
            "pedantic": {"level": "deny", "priority": -1},
            "disallowed_methods": "deny",
        },
    }


def _island_fixture() -> dict:
    return {
        "rust": {"unsafe_code": "allow"},
        "clippy": {
            "all": {"level": "deny", "priority": -1},
            "pedantic": {"level": "deny", "priority": -1},
            "disallowed_methods": "deny",
        },
    }


def self_test() -> list[str]:
    failures = []
    root = _root_fixture()

    def expect(label: str, lints: dict, extras: dict, want_any: bool) -> None:
        got = compare_island("fixture", root, lints, extras)
        if bool(got) != want_any:
            failures.append(
                f"{label}: expected {'a problem' if want_any else 'no problems'}, got {got!r}"
            )

    # The in-sync shape passes.
    expect("in sync", _island_fixture(), {}, False)

    # A dropped `pedantic` is caught — the exact falsification #1179 asked for.
    dropped = _island_fixture()
    del dropped["clippy"]["pedantic"]
    expect("pedantic deleted", dropped, {}, True)

    # A weakened level is caught even though the key is present.
    weakened = _island_fixture()
    weakened["clippy"]["pedantic"] = {"level": "warn", "priority": -1}
    expect("pedantic weakened to warn", weakened, {}, True)

    # A silently-changed priority is caught (value compared whole).
    repriorised = _island_fixture()
    repriorised["clippy"]["all"] = {"level": "deny", "priority": 0}
    expect("priority changed", repriorised, {}, True)

    # `unsafe_code` must be spelled `allow`, not inherited or forbidden.
    forbidding = _island_fixture()
    forbidding["rust"]["unsafe_code"] = "forbid"
    expect("island forbids unsafe", forbidding, {}, True)
    missing_unsafe = _island_fixture()
    del missing_unsafe["rust"]["unsafe_code"]
    expect("island omits unsafe_code", missing_unsafe, {}, True)

    # An undeclared extra allow is caught; a declared one passes.
    extra = _island_fixture()
    extra["clippy"]["needless_range_loop"] = "allow"
    expect("undeclared extra allow", extra, {}, True)
    expect(
        "declared extra allow",
        extra,
        {"clippy": {"needless_range_loop": "allow"}},
        False,
    )
    expect(
        "declared extra with a different value",
        extra,
        {"clippy": {"needless_range_loop": "deny"}},
        True,
    )

    # A whole table missing, and the inheriting shape, are both caught.
    no_clippy = _island_fixture()
    del no_clippy["clippy"]
    expect("no [lints.clippy] at all", no_clippy, {}, True)
    expect("island inherits instead of mirroring", {"workspace": True}, {}, True)

    # Rule 6, both directions.
    if check_non_island("fixture", {"workspace": True}):
        failures.append("check_non_island: flagged a correctly-inheriting member")
    if not check_non_island("fixture", {"rust": {"unsafe_code": "allow"}}):
        failures.append("check_non_island: missed a third hand-rolled lints table")
    if not check_non_island("fixture", {}):
        failures.append("check_non_island: missed a member with no [lints] at all")

    return failures


def main() -> int:
    failures = self_test()
    if failures:
        print("lints-tables scan: SELF-TEST FAILED", file=sys.stderr)
        for line in failures:
            print(f"  {line}", file=sys.stderr)
        print(
            "\nThe scanner disagrees with its own fixtures, so any verdict it gives on\n"
            "the tree is meaningless. Fix the comparison, not the fixtures.",
            file=sys.stderr,
        )
        return 2

    try:
        root_manifest = read_manifest(ROOT_MANIFEST)
        root_tables = root_lint_tables(root_manifest)
    except (OSError, tomllib.TOMLDecodeError, LookupError) as e:
        print(f"lints-tables scan: cannot read the root manifest: {e}", file=sys.stderr)
        print("  (run from inside the repository)", file=sys.stderr)
        return 2

    if root_tables["rust"].get(UNSAFE_CODE) != ROOT_UNSAFE_CODE:
        print(
            "lints-tables scan: root [workspace.lints.rust] "
            f"{UNSAFE_CODE} is {root_tables['rust'].get(UNSAFE_CODE)!r}, expected "
            f"{ROOT_UNSAFE_CODE!r} — the whole premise of this check "
            "(the islands are the exception) no longer holds",
            file=sys.stderr,
        )
        return 2

    members = root_manifest.get("workspace", {}).get("members", [])
    if not members:
        print("lints-tables scan: root Cargo.toml lists no workspace members", file=sys.stderr)
        return 2

    problems = []
    checked_islands = 0
    for member in sorted(members):
        manifest_path = os.path.join(REPO_ROOT, member, "Cargo.toml")
        try:
            manifest = read_manifest(manifest_path)
        except (OSError, tomllib.TOMLDecodeError) as e:
            print(f"lints-tables scan: cannot read {member}/Cargo.toml: {e}", file=sys.stderr)
            return 2
        lints = crate_lint_tables(manifest)
        if member in ISLANDS:
            name = ISLANDS[member]
            checked_islands += 1
            problems.extend(
                compare_island(name, root_tables, lints, EXTRA_ALLOWS.get(name, {}))
            )
        else:
            problems.extend(check_non_island(member, lints))

    if checked_islands != len(ISLANDS):
        print(
            f"lints-tables scan: expected {len(ISLANDS)} island(s) among the workspace "
            f"members, found {checked_islands} — ISLANDS in this script names a crate "
            "that is no longer a member",
            file=sys.stderr,
        )
        return 2

    if problems:
        print(f"\nERROR: {len(problems)} lint-table divergence(s):\n", file=sys.stderr)
        for line in problems:
            print(f"  - {line}", file=sys.stderr)
        print(
            "\nThe root `[workspace.lints]` table and the two unsafe islands' hand-mirrored\n"
            "copies must agree on every lint except `unsafe_code`. Cargo's workspace-lints\n"
            "inheritance is all-or-nothing, which is why the copies exist; nothing the\n"
            "compiler runs notices when one falls behind, which is why this check does.",
            file=sys.stderr,
        )
        return 1

    print(
        f"lints-tables scan: {len(ISLANDS)} island(s) mirror root's "
        f"{sum(len(t) for t in root_tables.values())} lint entries, "
        f"{len(members) - len(ISLANDS)} member(s) inherit them",
        flush=True,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
