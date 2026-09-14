#!/usr/bin/env python3
"""Fail if `nix/module-common.nix`'s hand-mirrored vocabulary drifts from Rust.

Two `config.*` subsystem families today — `core-leds` (#1041) and `agents`
(#1227 item 1) — plus one option that is not a `config.*` subsystem at all:
`programs.trollshell.plugins.<id>.mount` (#1161). The file was
`lint-core-leds-vocab.py` while there was only one family; the rename came
with the second (#1237 review MEDIUM-3), because the thing it guards is a
*hand-mirrored vocabulary* as such and a name that says `core-leds` would be
wrong nine more times.

THE DEFECT
----------
`programs.trollshell.config.core-leds` (`nix/module-common.nix`) hand-mirrors
three things from `core-leds.toml`'s Rust schema:

  - `style`'s enum — must list exactly `DisplayStyle::ALL`
    (`crates/hytte-preem/src/style.rs`), in `name()`'s spelling, in `ALL`'s
    order.
  - `fill`'s enum — must match `parse_core_leds_fill`'s accepted strings
    (`trollshell/src/config/core_leds.rs`); `Fill` has no `ALL`/`name()` to
    read this from directly, so the Rust source of truth here is that
    function's own match arms.
  - `rows`'s upper bound (`lib.types.ints.between 0 <n>`) — must equal
    [`MAX_ROWS`] (`trollshell/src/config/core_leds.rs`).

`programs.trollshell.config.agents` hand-mirrors two more from
`crates/hytte-plugin-agents/src/config.rs`:

  - `poll_seconds`'s bounds (`lib.types.ints.between <lo> <hi>`) — must equal
    `MIN_POLL_SECONDS`/`MAX_POLL_SECONDS`. Structurally the same defect as
    `rows` above; measured on #1237's own branch, moving `MAX_POLL_SECONDS`
    to 7200 left `cargo test`, the byte-fixture check and this script all
    green while the now-legal `poll_seconds = 7200` threw at nix eval.
  - the *set of keys itself* — every serde field of `AgentsConfig` and of
    `Display` must have a nix option leaf, and vice versa. This is the other
    direction of the same drift and the byte fixture cannot see it: a fixture
    pins the VALUES of one documented example, so a Rust field nix can never
    set (measured on the same branch: `pub chat_url: Option<String>`, every
    gate green) is invisible to it, and a nix option leaf the schema does not
    know only shows up as an `unknown_keys` warning in a log nobody reads.
    Compared as SETS, both ways, so a rename on either side reds twice rather
    than passing as an add plus a remove.

    The comparison is against the `#[serde]` fields rather than
    `DEFAULT_TOML`'s leaf keys — the fields are the complete surface
    (`DEFAULT_TOML` deliberately leaves `[display.*]` commented out, so its
    keys are a strict subset), and `DEFAULT_TOML` is already pinned against
    the struct by `the_shipped_default_parses_and_matches_the_rust_default`
    in that crate's own tests.

`programs.trollshell.plugins.<id>.mount` (#1161) hand-mirrors a third,
non-`config.*` vocabulary from `crates/hytte-plugin-proto/src/manifest.rs`:

  - the `types.enum` of nine wire names — must list exactly `Mount::ALL`, in
    `wire_name()`'s spelling and `ALL`'s order. Structurally the same rule as
    `style` above, and read with the same two functions (`bracket_list_after`
    nix-side, an `ALL` + name-table compose Rust-side); it is here because
    the mirror is here, not because the option is a config subsystem.

    Measured on #1260's own branch: renaming `"SidebarLead"` to
    `"SidebarHead"` and inserting a tenth value left **all seven** gates that
    could plausibly see it green — `hm-module-plugin-mount`,
    `nixos-module-plugin-mount`, `config-vocab`, `options-doc`, `hm-module`,
    `nixos-module` and this script. (The two mount eval checks look like a
    guard but only catch a rename of the one value their fixture happens to
    set.) The consequence on a user's box is worse than the `config.*`
    families': `mount = "SidebarHead"` passes nix eval, renders
    `HYTTE_PLUGIN_MOUNT=SidebarHead`, and the SDK then **refuses to start**
    the plugin — every boot, on every machine. The reverse direction (a
    rename in `wire_name` with the nix enum left stale) is the same outage.

Nothing fails if the two drift: a fifth `DisplayStyle` variant added Rust-side
renders a base file the nix option would reject at eval before anyone ever
gets to `core-leds.toml`; a style renamed Rust-side without the nix edit
renders a base file the shell then rejects per-key at load time, silently,
the first time anyone actually sets it. Since this option is explicitly "the
option shape every later subsystem family copies" (#1041), the mirror is
about to be hand-duplicated nine more times — this is the one place today
that would catch any of them drifting. `agents` is duplicate one of the
nine, and adding it here is what the rename above is for: a third family
adds a rule and a `*_RS` constant, not a second script.

WHY THIS IS A NIX LINT AND NOT A `cargo test` (#1081 review, second round)
---------------------------------------------------------------------------
The first version of this guard *was* a `cargo test` in
`trollshell/src/config/core_leds.rs`, reading `nix/module-common.nix` off
disk at test time via `CARGO_MANIFEST_DIR/../nix/module-common.nix`. It
passed locally and failed in CI: `nix/package.nix`'s crane source filter
(`nix/package.nix:79-85`) keeps only `.rs`/`.toml`/`Cargo.lock`,
`assets/hytte-ui/style.css`, and `.vert`/`.frag` files, so the sandboxed
`workspace` derivation `cargo test --workspace` runs inside has no
`nix/module-common.nix` at all — the exact `include_str!`-of-`assets/` trap
CLAUDE.md already documents, just reached by `std::fs::read_to_string`
instead of `include_str!`. Widening the crane filter to keep `*.nix` was
rejected: every edit to *any* `.nix` file would then invalidate the
`workspace` derivation's source hash and force a full recompile
(`nix/package.nix`'s own doc — the filter is deliberately narrow so nix-only
changes are free).

A flake check run by a plain `pkgs.runCommand` (the `bind-pins`/`glsl`
precedent, `nix/lint-bind-pins.py` / `nix/lint-glsl.py`) has the opposite
problem in a good way: it reads the *real repository tree* (there is no
crane filter between a `runCommand`'s `src = ./.;`-shaped input and the
checkout), needs no compile, and reds in seconds. That is what this script
is, wired into `flake.nix`'s `checks.config-vocab`.

Run it by hand from the repo root with:

    nix shell nixpkgs#python3 --command python3 nix/lint-config-vocab.py

The `nix shell` is not optional: **`python3` is deliberately not on the
devShell PATH** (see `nix/lint-bind-pins.py`'s own header), so a bare
`python3 nix/lint-config-vocab.py` is `command not found`.

WHY A HAND-ROLLED SCAN AND NOT A NIX-EVAL ROUND-TRIP
-----------------------------------------------------
Evaluating `nix/module-common.nix` for real (`nix eval` or an in-process
`nix-instantiate`) would need a full module-system `evalModules` call just to
read a handful of literals back out of two option declarations — heavier than the
thing it is checking, and it could not read the Rust side at all (Nix has no
Rust parser). A small hand-rolled scan of both files, in the
`lint-bind-pins.py` style, is simple enough here that it can name exactly
which anchor it failed to find rather than a bare "no match": both files are
short, `nix fmt`/`rustfmt` already normalise their formatting, and neither
vocabulary is nested more than one bracket deep.

USAGE
-----
    python3 nix/lint-config-vocab.py

Exits 0 when the two agree, 1 naming the first mismatch, 2 when the scan
itself is untrustworthy (a source file is missing, an anchor cannot be found,
or `self_test()` — run first, on every invocation — disagrees with its own
fixtures).
"""

import os
import re
import sys

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

STYLE_RS = os.path.join(REPO_ROOT, "crates", "hytte-preem", "src", "style.rs")
CORE_LEDS_RS = os.path.join(REPO_ROOT, "trollshell", "src", "config", "core_leds.rs")
AGENTS_RS = os.path.join(REPO_ROOT, "crates", "hytte-plugin-agents", "src", "config.rs")
MANIFEST_RS = os.path.join(REPO_ROOT, "crates", "hytte-plugin-proto", "src", "manifest.rs")
MODULE_COMMON_NIX = os.path.join(REPO_ROOT, "nix", "module-common.nix")

# `config.agents`' nix option leaves, paired with the Rust struct whose serde
# fields they must equal. One entry per *level* rather than one per subsystem:
# `display` is both a leaf of `AgentsConfig` (the key `display` itself) and an
# `attrsOf` submodule whose own options are `Display`'s fields.
AGENTS_STRUCT_LEVELS = (
    ("config.agents = lib.mkOption {", "AgentsConfig"),
    ("display = lib.mkOption {", "Display"),
)


def match_delim(src: str, start: int, open_c: str, close_c: str) -> int:
    """Index just past the delimiter matching the one at/after `start`."""
    depth = 0
    for i in range(start, len(src)):
        if src[i] == open_c:
            depth += 1
        elif src[i] == close_c:
            depth -= 1
            if depth == 0:
                return i + 1
    return -1


def bracket_list_after(src: str, anchor: str) -> list[str]:
    """The quoted strings inside the first `[ … ]` found after `anchor`.

    One bracket depth only — correct for the `nix fmt`-formatted enum lists
    this reads, one item per line.
    """
    start = src.find(anchor)
    if start < 0:
        raise LookupError(f"anchor {anchor!r} not found")
    after = src[start:]
    open_i = after.find("[")
    if open_i < 0:
        raise LookupError(f"no '[' found after anchor {anchor!r}")
    close_i = after.find("]", open_i)
    if close_i < 0:
        raise LookupError(f"no ']' found closing the list after anchor {anchor!r}")
    body = after[open_i + 1 : close_i]
    return [tok.strip('"') for tok in body.split() if tok.strip('"')]


def ints_between_after(src: str, anchor: str) -> tuple[int, int]:
    """The two integers of the FIRST `ints.between lo hi` at or after `anchor`.

    Anchor-relative rather than file-global on purpose: there is more than one
    `ints.between` in `nix/module-common.nix` now (`core-leds`' `rows` and
    `agents`' `poll_seconds`), so a caller must say which option's bound it
    means or it silently reads whichever happens to come first in the file.
    """
    start = src.find(anchor)
    if start < 0:
        raise LookupError(f"anchor {anchor!r} not found")
    call = src.find("ints.between ", start)
    if call < 0:
        raise LookupError(f"no `ints.between` found after anchor {anchor!r}")
    after = src[call + len("ints.between ") :]
    tokens = after.split()
    if len(tokens) < 2:
        raise LookupError(f"fewer than two integers after anchor {anchor!r}")

    def as_int(tok: str, which: str) -> int:
        # The upper bound carries whatever punctuation closes the expression:
        # `64)` when the call is wrapped in a `lib.types.either`, `3600);`
        # when it is the last thing on the line. Take the leading digits and
        # let anything else be a LookupError rather than a silent wrong read.
        m = re.match(r"(\d+)", tok)
        if not m:
            raise LookupError(
                f"the {which} bound after anchor {anchor!r} is not an integer: {tok!r}"
            )
        return int(m.group(1))

    return (as_int(tokens[0], "lower"), as_int(tokens[1], "upper"))


def _enum_all_names(src: str, ty: str, fn: str, where: str) -> list[str]:
    """`<ty>::ALL`'s variants mapped through `fn <fn>(self) -> &'static str`.

    Reads `pub const ALL: [Self; N] = [Self::Vfd, Self::Lcd, …];` (or the
    `[Mount; N] = [Mount::…]` spelling — both appear in the tree) for the
    variant *order*, then that function's match arms for the
    variant -> string mapping, and composes the two. This is exactly what
    `<ty>::ALL.iter().map(|v| v.<fn>())` computes at runtime, so the nix side
    is checked against the same sequence the Rust schema itself would
    resolve.

    Two enums go through this: `DisplayStyle::ALL`/`name` in
    `crates/hytte-preem/src/style.rs` and `Mount::ALL`/`wire_name` in
    `crates/hytte-plugin-proto/src/manifest.rs`. One function rather than two
    near-copies because the only differences are the type's spelling and the
    method's name — and a second copy would be a second thing to fix when
    either enum grows a shape this scan cannot follow.
    """
    qualified = rf"(?:Self|{ty})"
    m = re.search(rf"pub const ALL:\s*\[{qualified};\s*\d+\]\s*=\s*\[([^\]]*)\];", src)
    if not m:
        raise LookupError(f"{ty}::ALL not found in {where}")
    variants = [
        v.strip().removeprefix("Self::").removeprefix(f"{ty}::")
        for v in m.group(1).split(",")
        if v.strip()
    ]

    sig = re.search(rf"fn {fn}\(self\)\s*->\s*&'static str\s*\{{", src)
    if not sig:
        raise LookupError(f"{ty}::{fn}() not found in {where}")
    body_start = sig.end() - 1
    body_end = match_delim(src, body_start, "{", "}")
    if body_end < 0:
        raise LookupError(f"{ty}::{fn}()'s body brace never closes")
    body = src[body_start:body_end]

    name_of = dict(re.findall(rf"{qualified}::(\w+)\s*=>\s*\"([^\"]+)\"", body))
    missing = [v for v in variants if v not in name_of]
    if missing:
        raise LookupError(f"{fn}() has no arm for ALL variant(s): {missing}")
    return [name_of[v] for v in variants]


def display_style_all(src: str) -> list[str]:
    """`DisplayStyle`'s canonical spelling, in `ALL`'s order."""
    return _enum_all_names(src, "DisplayStyle", "name", "style.rs")


def mount_wire_names(src: str) -> list[str]:
    """`Mount`'s wire names, in `ALL`'s order (#1161, #1260 review F3).

    The vocabulary `programs.trollshell.plugins.<id>.mount`'s `types.enum`
    hand-mirrors, and the one the SDK matches `HYTTE_PLUGIN_MOUNT` against at
    plugin startup — `Mount::from_wire_name` is `wire_name`'s exact inverse,
    so a value outside this list is a launch failure rather than a card in
    the wrong place.
    """
    return _enum_all_names(src, "Mount", "wire_name", "crates/hytte-plugin-proto/src/manifest.rs")


def fill_parser_vocab(src: str) -> list[str]:
    """`parse_core_leds_fill`'s accepted strings, in source order.

    `Fill` (`crates/hytte-preem/src/led_matrix.rs`) has no `ALL`/`name()` to
    read this from the way `DisplayStyle` does, so the source of truth is
    this parser's own match arms — the single judge `core_leds.rs`'s own
    module doc already calls it.
    """
    fn = re.search(r"fn parse_core_leds_fill\(raw: &str\) -> Result<Fill, &str>\s*\{", src)
    if not fn:
        raise LookupError("parse_core_leds_fill not found in core_leds.rs")
    body_start = fn.end() - 1
    body_end = match_delim(src, body_start, "{", "}")
    if body_end < 0:
        raise LookupError("parse_core_leds_fill's body brace never closes")
    body = src[body_start:body_end]
    return re.findall(r'"([^"]+)"\s*=>\s*Ok\(Fill::', body)


def option_block(src: str, anchor: str) -> str:
    """The brace-matched body of the option declaration named by `anchor`.

    `anchor` ends with the declaration's own opening brace (e.g.
    ``"display = lib.mkOption {"``), so this returns everything from that
    brace to the one matching it — the whole `mkOption` call, nested
    submodule options included.
    """
    start = src.find(anchor)
    if start < 0:
        raise LookupError(f"anchor {anchor!r} not found")
    open_i = start + len(anchor) - 1
    if src[open_i] != "{":
        raise LookupError(f"anchor {anchor!r} does not end at an opening brace")
    end = match_delim(src, open_i, "{", "}")
    if end < 0:
        raise LookupError(f"the block opened by anchor {anchor!r} never closes")
    return src[open_i:end]


def option_leaves(body: str) -> list[str]:
    """Every `<name> = lib.mkOption {` declared inside `body`, in source order."""
    return re.findall(r"^\s*(\w+) = lib\.mkOption \{", body, re.M)


def agents_option_levels(nix_src: str) -> dict[str, list[str]]:
    """`config.agents`' option leaves, one list per Rust struct level.

    The nested `display` submodule's own options (`label`/`icon`/`project`)
    belong to `Display`, not to `AgentsConfig`, so the nested block is lifted
    out and replaced by an empty one before the outer level is read — leaving
    `display` itself counted exactly once, as the `AgentsConfig` field it is.
    """
    agents_anchor, _ = AGENTS_STRUCT_LEVELS[0]
    display_anchor, _ = AGENTS_STRUCT_LEVELS[1]
    agents_body = option_block(nix_src, agents_anchor)
    display_body = option_block(agents_body, display_anchor)
    outer = agents_body.replace(display_body, "{ }", 1)
    return {"AgentsConfig": option_leaves(outer), "Display": option_leaves(display_body)}


def _serde_attr_lists(text: str) -> list[str]:
    """Every `#[serde(...)]` attribute's inner content found in `text`."""
    return re.findall(r"#\[serde\(([^)]*)\)\]", text)


def _serde_has(text: str, pattern: str) -> bool:
    """Whether `pattern` matches ANYWHERE inside any `#[serde(...)]`
    attribute list in `text` — scanned as a whole list rather than by
    position (#1241). The idiom `Display` itself uses,
    `#[serde(default, skip_serializing_if = "…", rename = "glyph")]`, puts
    `rename` third; a scan that only checked the first entry or two (the old
    `"serde(rename" in body` / `"serde(default, rename" in body` prefixes)
    scanned that green.
    """
    return any(re.search(pattern, attrs) for attrs in _serde_attr_lists(text))


def struct_serde_fields(src: str, struct: str) -> list[str]:
    """The serde-visible field names of `pub struct <struct>`, in source order.

    Raises rather than guessing if the struct (or its container attributes)
    uses a serde spelling this scan cannot follow — `rename`, `rename_all` and
    `flatten` all make the wire name something other than the Rust field name,
    and a scan that quietly ignored them would compare the nix side against
    names that never appear in the TOML. An untrustworthy verdict is worth
    exit 2, not a green.
    """
    m = re.search(rf"pub struct {struct}\b[^{{]*\{{", src)
    if not m:
        raise LookupError(f"`pub struct {struct}` not found")
    # Container attributes sit between the doc comment and the struct keyword;
    # 500 characters back covers the derive list and any `#[serde(...)]` line.
    head = src[max(0, m.start() - 500) : m.start()]
    if _serde_has(head, r"rename_all"):
        raise LookupError(f"`{struct}` carries a serde `rename_all` this scan cannot follow")
    body_start = m.end() - 1
    body_end = match_delim(src, body_start, "{", "}")
    if body_end < 0:
        raise LookupError(f"`pub struct {struct}`'s body brace never closes")
    body = src[body_start:body_end]
    if _serde_has(body, r"rename\s*="):
        raise LookupError(f"`{struct}` uses serde `rename`, which this scan cannot follow")
    if _serde_has(body, r"\bflatten\b"):
        raise LookupError(f"`{struct}` uses serde `flatten`, which this scan cannot follow")
    # `pub(crate)`/`pub(super)` is still a `pub` field as far as serde and
    # TOML are concerned — only a Rust-side visibility restriction, which
    # this scan must not confuse with "not a field at all" (#1241): the old
    # `pub (\w+):` pattern had a literal space and so never matched a
    # visibility qualifier, silently dropping the field from the comparison
    # instead of comparing it.
    fields = re.findall(r"pub(?:\([^)]*\))?\s+(\w+):", body)
    if not fields:
        raise LookupError(f"`pub struct {struct}` has no `pub` fields")
    return fields


def poll_seconds_bounds(src: str) -> tuple[int, int]:
    """`MIN_POLL_SECONDS`/`MAX_POLL_SECONDS` from hytte-plugin-agents' config.rs."""

    def one(name: str) -> int:
        m = re.search(rf"pub const {name}:\s*u64\s*=\s*(\d+)\s*;", src)
        if not m:
            raise LookupError(f"{name} not found in crates/hytte-plugin-agents/src/config.rs")
        return int(m.group(1))

    return (one("MIN_POLL_SECONDS"), one("MAX_POLL_SECONDS"))


def max_rows(src: str) -> int:
    m = re.search(r"const MAX_ROWS:\s*usize\s*=\s*(\d+)\s*;", src)
    if not m:
        raise LookupError("MAX_ROWS not found in core_leds.rs")
    return int(m.group(1))


# Fixtures for `self_test()`, run on every invocation. A clean tree proves
# nothing about whether the extraction functions still work — only a case
# built to disagree can tell the two apart (the same reasoning
# `lint-bind-pins.py`'s header gives for its own fixtures).
def self_test() -> list[str]:
    failures = []

    style_src = """
    pub const ALL: [Self; 4] = [Self::Vfd, Self::Lcd, Self::Oled, Self::Crt];
    pub fn name(self) -> &'static str {
        match self {
            Self::Vfd => "vfd",
            Self::Lcd => "lcd",
            Self::Oled => "oled",
            Self::Crt => "crt",
        }
    }
    """
    got = display_style_all(style_src)
    if got != ["vfd", "lcd", "oled", "crt"]:
        failures.append(f"display_style_all: expected the four in ALL's order, got {got}")

    fill_src = """
    fn parse_core_leds_fill(raw: &str) -> Result<Fill, &str> {
        match raw {
            "spare" => Ok(Fill::Spare),
            "blank" => Ok(Fill::Blank),
            other => Err(other),
        }
    }
    """
    got = fill_parser_vocab(fill_src)
    if got != ["spare", "blank"]:
        failures.append(f"fill_parser_vocab: expected [spare, blank], got {got}")
    if "other" in got:
        failures.append("fill_parser_vocab: the catch-all arm must never be captured")

    # #1161/#1260 F3: `Mount::ALL` is spelled `[Mount; N] = [Mount::…]`, not
    # `[Self; N] = [Self::…]` the way `DisplayStyle::ALL` is, and its name
    # table is `wire_name` rather than `name`. Both spellings must read.
    mount_src = """
    pub const ALL: [Mount; 3] = [Mount::SidebarLead, Mount::BarLeft, Mount::BarRight];
    #[must_use]
    pub fn wire_name(self) -> &'static str {
        match self {
            Mount::SidebarLead => "SidebarLead",
            Mount::BarLeft => "BarLeft",
            Mount::BarRight => "BarRight",
        }
    }
    """
    got = mount_wire_names(mount_src)
    if got != ["SidebarLead", "BarLeft", "BarRight"]:
        failures.append(f"mount_wire_names: expected the three in ALL's order, got {got}")
    # A half-done append — a variant in `ALL` with no arm in the name table —
    # must refuse rather than silently drop it from the comparison, which
    # would let the nix enum go one value short and stay green.
    half_done = mount_src.replace('Mount::BarRight => "BarRight",', "")
    try:
        mount_wire_names(half_done)
        failures.append("mount_wire_names: a variant with no wire_name arm was not refused")
    except LookupError:
        pass
    # The two enums must not read each other's tables: `name`/`wire_name` and
    # `DisplayStyle`/`Mount` are both parameters, so a mix-up would show up
    # as one of them silently reading the other's arms out of a file that
    # holds both. Neither function may find anything in the other's source.
    for fn_name, fn, other_src in (
        ("display_style_all", display_style_all, mount_src),
        ("mount_wire_names", mount_wire_names, style_src),
    ):
        try:
            fn(other_src)
            failures.append(f"{fn_name}: read the other enum's table instead of refusing")
        except LookupError:
            pass

    if max_rows("const MAX_ROWS: usize = 64;") != 64:
        failures.append("max_rows: did not read the literal back")
    # A number sharing digits with a nearby, differently-named constant must
    # not be picked up instead.
    if max_rows("const OTHER_ROWS: usize = 6;\nconst MAX_ROWS: usize = 64;") != 64:
        failures.append("max_rows: matched the wrong constant")

    nix_src = """
    style = lib.mkOption {
      type = lib.types.nullOr (
        lib.types.enum [
          "vfd"
          "lcd"
          "oled"
          "crt"
        ]
      );
    };
    rows = lib.mkOption {
      type = lib.types.nullOr (
        lib.types.either (lib.types.ints.between 0 64) (lib.types.enum [ "rect" ])
      );
    };
    """
    got = bracket_list_after(nix_src, "style = lib.mkOption {")
    if got != ["vfd", "lcd", "oled", "crt"]:
        failures.append(f"bracket_list_after: expected the four nix-side, got {got}")

    # #1161's `mount` enum is nested one level deeper than `style`'s
    # (`nullOr (enum [ … ])` either way, but written across more lines), so
    # read a fixture shaped like the real option rather than assuming.
    mount_nix_src = """
    mount = lib.mkOption {
      type = lib.types.nullOr (
        lib.types.enum [
          "SidebarLead"
          "BarLeft"
          "BarRight"
        ]
      );
      default = null;
    };
    """
    got = bracket_list_after(mount_nix_src, "mount = lib.mkOption {")
    if got != ["SidebarLead", "BarLeft", "BarRight"]:
        failures.append(f"bracket_list_after: expected the three mount names, got {got}")
    got_bounds = ints_between_after(nix_src, "ints.between ")
    if got_bounds != (0, 64):
        failures.append(f"ints_between_after: expected (0, 64), got {got_bounds}")

    # Two `ints.between` calls in one file: reading a bound must follow the
    # anchor it was given, not "the first one in the file". Without this, the
    # `agents` rule below would silently re-check `core-leds`' `rows` bound.
    two_bounds_src = """
    rows = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.between 0 64);
    };
    poll_seconds = lib.mkOption {
      type = lib.types.nullOr (lib.types.ints.between 1 3600);
    };
    """
    got_bounds = ints_between_after(two_bounds_src, "poll_seconds = lib.mkOption {")
    if got_bounds != (1, 3600):
        failures.append(f"ints_between_after: anchor ignored, expected (1, 3600), got {got_bounds}")
    got_bounds = ints_between_after(two_bounds_src, "rows = lib.mkOption {")
    if got_bounds != (0, 64):
        failures.append(f"ints_between_after: expected (0, 64) for rows, got {got_bounds}")

    if poll_seconds_bounds(
        "pub const MIN_POLL_SECONDS: u64 = 1;\npub const MAX_POLL_SECONDS: u64 = 3600;\n"
    ) != (1, 3600):
        failures.append("poll_seconds_bounds: did not read the two literals back")
    if poll_seconds_bounds(
        "pub const DEFAULT_POLL_SECONDS: u64 = 2;\n"
        "pub const MIN_POLL_SECONDS: u64 = 1;\n"
        "pub const MAX_POLL_SECONDS: u64 = 3600;\n"
    ) != (1, 3600):
        failures.append("poll_seconds_bounds: matched a neighbouring constant instead")

    struct_src = '''
    #[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
    pub struct Display {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub label: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub icon: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub project: Option<String>,
    }
    #[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
    pub struct AgentsConfig {
        #[serde(default = "default_socket")]
        pub socket: String,
        #[serde(default = "default_poll_seconds")]
        pub poll_seconds: u64,
        #[serde(default)]
        pub display: BTreeMap<String, Display>,
    }
    '''
    got = struct_serde_fields(struct_src, "Display")
    if got != ["label", "icon", "project"]:
        failures.append(f"struct_serde_fields: expected Display's three, got {got}")
    got = struct_serde_fields(struct_src, "AgentsConfig")
    if got != ["socket", "poll_seconds", "display"]:
        failures.append(f"struct_serde_fields: expected AgentsConfig's three, got {got}")
    # A struct must not absorb the fields of the one declared after it — the
    # brace match is what keeps the two levels apart.
    if "label" in struct_serde_fields(struct_src, "AgentsConfig"):
        failures.append("struct_serde_fields: AgentsConfig swallowed a neighbouring struct")
    # A serde spelling that renames the wire key must make the scan refuse
    # rather than compare the nix side against a name TOML never sees.
    renamed = '''
    #[derive(Deserialize)]
    #[serde(rename_all = "kebab-case")]
    pub struct Renamed {
        pub poll_seconds: u64,
    }
    '''
    try:
        struct_serde_fields(renamed, "Renamed")
        failures.append("struct_serde_fields: a serde rename_all was not refused")
    except LookupError:
        pass

    # #1241: `rename` inside a MULTI-attribute serde list, not just as the
    # sole or leading entry — the `Display` idiom itself,
    # `#[serde(default, skip_serializing_if = "…", rename = "glyph")]`,
    # scanned green before this fix (the old check only looked at the
    # attribute's first one or two entries).
    multi_attr_renamed = '''
    #[derive(Deserialize)]
    pub struct MultiAttrRenamed {
        #[serde(default, skip_serializing_if = "Option::is_none", rename = "glyph")]
        pub icon: Option<String>,
    }
    '''
    try:
        struct_serde_fields(multi_attr_renamed, "MultiAttrRenamed")
        failures.append("struct_serde_fields: a multi-attribute serde rename was not refused")
    except LookupError:
        pass

    # #1241: `pub(crate)`/`pub(super)` is still a field as far as serde and
    # TOML are concerned — the old `pub (\\w+):` pattern (note the literal
    # space) never matched a visibility qualifier at all, silently dropping
    # the field from the comparison instead of comparing it.
    scoped_visibility = '''
    #[derive(Deserialize)]
    pub struct ScopedVisibility {
        pub(crate) glyph: String,
        pub(super) label: String,
        pub plain: String,
    }
    '''
    got = struct_serde_fields(scoped_visibility, "ScopedVisibility")
    if got != ["glyph", "label", "plain"]:
        failures.append(
            f"struct_serde_fields: pub(crate)/pub(super) fields dropped, got {got}"
        )

    agents_nix_src = '''
    config.agents = lib.mkOption {
      type = lib.types.submodule {
        options = {
          socket = lib.mkOption {
            type = lib.types.nullOr (lib.types.strMatching "^/.+");
          };
          poll_seconds = lib.mkOption {
            type = lib.types.nullOr (lib.types.ints.between 1 3600);
          };
          display = lib.mkOption {
            type = lib.types.attrsOf (
              lib.types.submodule {
                options = {
                  label = lib.mkOption { type = lib.types.nullOr lib.types.str; };
                  icon = lib.mkOption { type = lib.types.nullOr lib.types.str; };
                  project = lib.mkOption { type = lib.types.nullOr lib.types.str; };
                };
              }
            );
          };
        };
      };
    };
    '''
    levels = agents_option_levels(agents_nix_src)
    if levels["AgentsConfig"] != ["socket", "poll_seconds", "display"]:
        failures.append(f"agents_option_levels: outer level wrong, got {levels['AgentsConfig']}")
    if levels["Display"] != ["label", "icon", "project"]:
        failures.append(f"agents_option_levels: nested level wrong, got {levels['Display']}")
    # The nested submodule's own options must not be counted as the
    # subsystem's own keys — that is the whole reason the block is lifted out.
    if set(levels["AgentsConfig"]) & set(levels["Display"]):
        failures.append("agents_option_levels: the two levels overlap")

    return failures


def read(path: str) -> str:
    with open(path, encoding="utf-8") as fh:
        return fh.read()


def _self_test_failed(lines: list[str]) -> int:
    """Print the "scanner disagrees with its own fixtures" verdict and return
    the exit-2 self-test-failure code the sibling scripts
    (`lint-bind-pins.py`, `lint-lints-tables.py`) reserve for it.

    One helper for `self_test()`'s two failure shapes — a fixture producing
    the wrong answer (`lines` is `self_test()`'s own return value) and a
    fixture raising outright (`lines` is the caught exception's message,
    wrapped by the caller) — so the two cannot read differently to whoever is
    staring at CI output. Before this, only the first shape went through
    here: the second escaped `main()` as an uncaught traceback and Python's
    default exit 1 — indistinguishable from `mount_wire_names` and friends
    finding a *real* drift in the tree, the one thing this exit code must
    never be confused with (#1270, inherited nit from the #1260 review).
    """
    print("config-vocab scan: SELF-TEST FAILED", file=sys.stderr)
    for line in lines:
        print(f"  {line}", file=sys.stderr)
    print(
        "\nThe scanner disagrees with its own fixtures, so any verdict it gives on the\n"
        "tree is meaningless. Fix the extraction functions rather than the fixtures.",
        file=sys.stderr,
    )
    return 2


def main() -> int:
    try:
        failures = self_test()
    except Exception as e:
        # Anything escaping self_test() means the scanner itself is broken,
        # not that the tree has real vocab drift -- narrower than Exception
        # missed whichever class the next fixture happened to raise (N1,
        # #1279 review): the three numeric readers (ints_between_after,
        # poll_seconds_bounds, max_rows) can raise ValueError as easily as
        # the extractors raise LookupError, and self_test() exercises all of
        # them.
        return _self_test_failed([f"a fixture raised {type(e).__name__}: {e}"])
    if failures:
        return _self_test_failed(failures)

    missing = [
        p
        for p in (STYLE_RS, CORE_LEDS_RS, AGENTS_RS, MANIFEST_RS, MODULE_COMMON_NIX)
        if not os.path.isfile(p)
    ]
    if missing:
        print(f"config-vocab scan: file(s) not found: {', '.join(missing)}", file=sys.stderr)
        print("  (run from inside the repository)", file=sys.stderr)
        return 2

    style_src = read(STYLE_RS)
    core_leds_src = read(CORE_LEDS_RS)
    agents_src = read(AGENTS_RS)
    manifest_src = read(MANIFEST_RS)
    nix_src = read(MODULE_COMMON_NIX)

    try:
        rust_style = display_style_all(style_src)
        nix_style = bracket_list_after(nix_src, "style = lib.mkOption {")
        rust_fill = fill_parser_vocab(core_leds_src)
        nix_fill = bracket_list_after(nix_src, "fill = lib.mkOption {")
        rust_max_rows = max_rows(core_leds_src)
        nix_rows_lo, nix_rows_hi = ints_between_after(nix_src, "rows = lib.mkOption {")
        rust_poll_lo, rust_poll_hi = poll_seconds_bounds(agents_src)
        nix_poll_lo, nix_poll_hi = ints_between_after(nix_src, "poll_seconds = lib.mkOption {")
        rust_mount = mount_wire_names(manifest_src)
        nix_mount = bracket_list_after(nix_src, "mount = lib.mkOption {")
        nix_agents_levels = agents_option_levels(nix_src)
        rust_agents_levels = {
            struct: struct_serde_fields(agents_src, struct)
            for _, struct in AGENTS_STRUCT_LEVELS
        }
    except LookupError as e:
        print(f"config-vocab scan: {e}", file=sys.stderr)
        print(
            "  (an anchor this script depends on has moved or been reworded — "
            "update nix/lint-config-vocab.py to match)",
            file=sys.stderr,
        )
        return 2

    mismatches = []
    if nix_style != rust_style:
        mismatches.append(
            f"style: nix/module-common.nix has {nix_style}, "
            f"but DisplayStyle::ALL is {rust_style}"
        )
    if nix_fill != rust_fill:
        mismatches.append(
            f"fill: nix/module-common.nix has {nix_fill}, "
            f"but parse_core_leds_fill accepts {rust_fill}"
        )
    if nix_rows_lo != 0:
        mismatches.append(
            f"rows: nix/module-common.nix's lower bound is {nix_rows_lo}, expected 0 "
            "(the file's own spelling of `rect`)"
        )
    if nix_rows_hi != rust_max_rows:
        mismatches.append(
            f"rows: nix/module-common.nix's upper bound is {nix_rows_hi}, "
            f"but MAX_ROWS is {rust_max_rows}"
        )
    if nix_mount != rust_mount:
        mismatches.append(
            f"mount: nix/module-common.nix has {nix_mount}, "
            f"but Mount::ALL is {rust_mount} "
            "(crates/hytte-plugin-proto/src/manifest.rs) — a value only one side "
            "knows renders a HYTTE_PLUGIN_MOUNT the SDK refuses at plugin startup"
        )
    if (nix_poll_lo, nix_poll_hi) != (rust_poll_lo, rust_poll_hi):
        mismatches.append(
            f"poll_seconds: nix/module-common.nix bounds it {nix_poll_lo}..{nix_poll_hi}, "
            f"but MIN_POLL_SECONDS/MAX_POLL_SECONDS are "
            f"{rust_poll_lo}/{rust_poll_hi} (crates/hytte-plugin-agents/src/config.rs)"
        )
    for _, struct in AGENTS_STRUCT_LEVELS:
        nix_keys = set(nix_agents_levels[struct])
        rust_keys = set(rust_agents_levels[struct])
        only_rust = sorted(rust_keys - nix_keys)
        only_nix = sorted(nix_keys - rust_keys)
        if only_rust:
            mismatches.append(
                f"{struct}: field(s) {only_rust} exist in "
                f"crates/hytte-plugin-agents/src/config.rs but have no "
                f"`programs.trollshell.config.agents` option leaf — nix can never set them"
            )
        if only_nix:
            mismatches.append(
                f"{struct}: option leaf(s) {only_nix} exist under "
                f"`programs.trollshell.config.agents` but are not fields of `{struct}` — "
                f"rendering them would only produce an unknown-key warning at load time"
            )

    if mismatches:
        print(
            f"\nERROR: {len(mismatches)} config vocabulary mismatch(es) between "
            "nix/module-common.nix and trollshell's Rust schemas:\n",
            file=sys.stderr,
        )
        for line in mismatches:
            print(f"  - {line}", file=sys.stderr)
        print(
            "\nnix/module-common.nix's `programs.trollshell.config.{core-leds,agents}` and "
            "`plugins.<id>.mount`\nhand-mirror this vocabulary (see those options' own "
            "descriptions) — update whichever side\nfell behind so a base-layer render and "
            "the shell's own parser agree.",
            file=sys.stderr,
        )
        return 1

    print(
        f"config-vocab scan: core-leds style {rust_style}, fill {rust_fill}, "
        f"rows 0-{rust_max_rows}; agents poll_seconds {rust_poll_lo}-{rust_poll_hi}, "
        f"keys {rust_agents_levels['AgentsConfig']} + display "
        f"{rust_agents_levels['Display']}; plugins.<id>.mount {rust_mount} "
        "— nix and Rust agree",
        flush=True,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
