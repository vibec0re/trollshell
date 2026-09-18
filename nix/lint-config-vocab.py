#!/usr/bin/env python3
"""Fail if `nix/module-common.nix`'s hand-mirrored vocabulary drifts from the
`Schema` consts `hytte-config`'s families declare (#888 P3, #1375).

THE SHAPE, SINCE #1358
-----------------------
Before #1358 there was no single Rust declaration of a config family's
leaves, so this script scraped whatever *did* exist: `DisplayStyle::ALL` and
`parse_core_leds_fill`'s match arms out of two different files,
`MAX_ROWS`/`MIN_POLL_SECONDS`/`MAX_POLL_SECONDS` out of a third, and the serde
field names of `AgentsConfig`/`Display` structs by hand-detecting `pub`,
`rename` and `flatten`. Four scraping strategies for four different shapes of
"the vocabulary", none of them the same shape `hytte_config::schema::verify`
(`crates/hytte-config/src/schema.rs`) already uses in-tree.

Since #1358 every `Subsystem` family that has one carries **one** declaration
of exactly that vocabulary — its `SCHEMA` const:

    crates/hytte-config-families/src/core_leds.rs:29    (core-leds, 4 fields)
    crates/hytte-config-families/src/workspaces.rs:35   (workspaces, 8 fields)
    crates/hytte-plugin-agents/src/config.rs:121         (agents, 6 fields)
    crates/hytte-plugin-stats/src/config.rs:290          (stats, 16 fields)

`Schema { family, fields: &[Field { path, kind: Kind, doc }] }`, where `Kind`
is `Bool | Int { min, max, also } | Choice(&[…]) | Color(&[…]) |
Text { blank_ok } | List(&Kind) | Map(&[Field])` — the full grammar is in
`crates/hytte-config/src/schema.rs:104-200`. Each family's own `#[test]`
already pins its `SCHEMA`'s bounds and vocabularies to its own parser
(`schema::verify` against `DEFAULT_TOML`, plus each plugin family's own
`the_schemas_fields_are_the_serde_surface`-shaped test) — that is Rust's job,
proven by `cargo test`, and this script does not repeat it. What is left for
THIS script, which no compile in the flake can see, is the other edge: does
`nix/module-common.nix`'s hand-typed mirror of that same `SCHEMA` still say
the same thing — same leaves, same bounds/enums, same first sentence of docs?

So this script reads **only** the four `SCHEMA` consts and
`nix/module-common.nix`. It no longer reads `hytte-preem/src/style.rs`,
`trollshell/src/config/core_leds.rs`'s parser, or
`crates/hytte-plugin-agents/src/config.rs`'s raw struct fields — the
parser↔schema edge those fed is Rust's job now, not this one's. Retired
outright: `fill_parser_vocab`, `display_style_all`/`_enum_all_names`-for-
`DisplayStyle`, `max_rows`, `poll_seconds_bounds`, `agents_option_levels`,
and the `struct_serde_fields` arm that read `AgentsConfig`/`Display` — every
Rust path this script no longer needs to read to answer the same question.

TWO ARMS THAT STAY EXACTLY AS THEY WERE
-----------------------------------------
`programs.trollshell.config.places` (`nix/module-common.nix:1507`) is **not**
a `Subsystem` family and has no `Schema` — `places.rs` keeps its own private
`PlaceCfg`/`DeparturesCfg` reader (see `hytte-config`'s crate docs). Its own
rule (#1339 item 2, `struct_serde_fields`/`places_option_levels`/
`PRIVATE_STRUCTS`/`compare_places` below) is untouched by #1375: same
functions, same messages, same fixtures.

`programs.trollshell.plugins.<id>.mount` (#1161) hand-mirrors a third,
non-`config.*` vocabulary — `Mount::ALL`/`wire_name()`
(`crates/hytte-plugin-proto/src/manifest.rs`) — and is not a config family
either. `_enum_all_names`/`mount_wire_names`/`bracket_list_after` and the
mount comparison in `main()` are unchanged from before #1375; see their own
docstrings for the history (#1161, #1260 review F3).

THE GRAMMAR THIS SCRIPT PARSES (text, not a compiler — see "WHY A HAND-ROLLED
SCAN" below; `nix/lint-bind-pins.py`'s brace/paren-matching precedent)
-------------------------------------------------------------------------
A `Field { path: "…", kind: <Kind-expr>, doc: "…" }` literal, found either
directly inside `pub const SCHEMA: Schema = Schema { family: …, fields: <expr>
};` (`agents`' shape — the array is inline) or by following `fields:` to an
identifier naming a sibling `const NAME: &[Field] = &[ … ];` in the same file
(`core-leds`/`workspaces`' shape). A `<Kind-expr>` is one of `Kind::Bool`,
`Kind::Int { min, max, also }` (each of `min`/`max` a literal integer OR a
bare `CONST_NAME`/`CONST_NAME.cast_signed()` resolved against a sibling `pub
const NAME: u64 = …;` — `agents.poll_seconds` and `stats`' `POLL_SECONDS`
alias both restate `MIN_POLL_SECONDS`/`MAX_POLL_SECONDS` this way rather than
a second pair of literals), `Kind::Choice { options: &[…] }`,
`Kind::Color { options: &[…] }`, `Kind::Text { blank_ok: bool }`,
`Kind::List(<Kind-expr-or-&IDENT>)` and `Kind::Map(<IDENT>)` (or an inline
`&[Field { … }, …]`), where `IDENT` is resolved the same way `fields:` is —
either a sibling `const NAME: &[Field] = &[ … ];` (`workspaces`' `STACK_FIELDS`/
`APP_FIELDS`, `agents`' `DISPLAY_FIELDS`) or, for a bare `Kind` behind a
`List`, a sibling `const NAME: Kind = <Kind-expr>;` alias (`workspaces`' `APP`,
`stats`' `POLL_SECONDS`). One recursive-descent parser (`parse_kind`/
`parse_field_literal`/`RustCtx`) walks all of it; there is no second copy for
a second file. Splitting on a `,`/`:` respects string literals and nested
`{}[]()` (`split_top_level`), so a doc sentence containing a colon or a comma
inside a `Kind::Choice`'s bracket cannot desync the parse — see
`split_top_level`'s own docstring.

`stats`' SCHEMA is the one deliberate exception: `card_fields!("sidebar",
"bar")` (`crates/hytte-plugin-stats/src/config.rs:218-263`) macro-expands one
eight-field template — `path: concat!($table, ".cpu")`, etc. — over the two
table names, for the sixteen leaves the file's own doc comment names
("What the sixteen leaves of `stats.toml` are"). `parse_card_fields_macro`
parses the macro's `$( … )*` template body once (the same `Field`/`Kind`
grammar above, with one extra path shape: `concat!($table, "…")` instead of a
bare string literal), reads the invocation's table-name arguments, and
materialises `{table}.cpu` → `"sidebar.cpu"`/`"bar.cpu"` for each. This is
read for its own sake — ask #1 ("the schema-parser must be shown to see every
field") and the zero-fields guard in `run_real_scan` both cover it — but its
sixteen leaves are never compared against nix (see "FAMILIES WITH NO NIX
SURFACE" below), so a parse bug here cannot itself turn a real drift green.

FAMILIES WITH NO NIX SURFACE: `stats`, `workspaces`
-----------------------------------------------------
Neither has a `programs.trollshell.config.<family>` block in
`nix/module-common.nix` today (grepped: zero hits for `config.workspaces` or
`config.stats`). Both `SCHEMA`s are still parsed — so a broken parse or a
family that shrinks to zero fields still reds `run_real_scan` — but neither
is compared against anything nix-side; the scan's own success line marks each
`skipped: no nix surface`. #1374 is the standing proposal to give them one
(generate `programs.trollshell.config.{stats,workspaces}` from the schema
rather than hand-typing a ninth and tenth mirror); when it lands, add both to
`FAMILY_NIX_ANCHORS` below and this skip goes away on its own.

THE COMPARISON (ask #2)
-------------------------
For each family with a nix block, the schema's leaf set and the nix option
leaf set (read at every nesting level a `Kind::Map`/`Kind::List(Map)` opens,
recursively — `compare()`) must be the SAME SET, both directions (a Rust
field with no nix leaf is drift nix can never set; a nix leaf with no Rust
field only ever warns at load time and is drift too). For fields present on
both sides, the nix `type` expression must match what the `Kind` says:

    Kind::Bool               lib.types.bool
    Kind::Int{min,max,also=[]}  lib.types.ints.between min max
    Kind::Int{…,also=[w,…]}   lib.types.either (ints.between min max) (enum [w,…])
    Kind::Choice{options}     lib.types.enum options   (same set, same order)
    Kind::Text{..}            lib.types.str  (or a STRICTER lib.types.strMatching "…")
    Kind::Color{..}           lib.types.str  (open string — see below)
    Kind::Map(fields)         lib.types.attrsOf (lib.types.submodule { options = …; })
    Kind::List(elem)          lib.types.listOf …

`Kind::Color` is not in ask #2's own table (only `Bool`/`Int`/`Choice`/`Text`/
`Map`/`List` are named there) — this script folds it into the same rule as
`Text`, because that is what `core-leds.color`'s own nix comment already
argues ("a plain string rather than an enum because the accepted vocabulary
is open-ended… any 6-digit hex triplet") and what the option actually renders
(`nullOr str`, not an enum): a closed `Kind::Choice`-shaped check here would
be a NEW, wrong rule against a deliberate, already-documented design, not a
gap this script should close by inventing an enum nobody wants.

`Text`/`Color` accepting a nix `strMatching` as well as a plain `str` is the
other place this script is looser than a literal reading of ask #2:
`agents.socket` is `Kind::Text { blank_ok: false }` but its nix type is
`lib.types.nullOr (lib.types.strMatching "^/.+")`, STRICTER than plain `str`
on purpose (that option's own comment: checked at nix eval because a bad
`socket` fails the whole file, unlike `core-leds.color`'s per-key failure). A
rule that only accepted bare `str` would red on a value the design explicitly
wants tighter than the schema's own floor; accepting either — never the other
direction, a `Kind::Choice` never satisfies a bare `str` requirement and vice
versa — is what makes this a hand-mirror check rather than a hand-mirror
REQUIREMENT nobody asked for.

`nullOr` wrapping everywhere is the nix side's spelling of "no opinion, defer
to a lower layer" (#1227) and is allowed on every field regardless of `Kind`,
same as before.

THE DOC GATE (ask #2's second half)
--------------------------------------
The nix `description`'s FIRST SENTENCE — up to the first `.` followed by
whitespace or end of string, after stripping backticks and `**bold**`/`*italic*`
markdown emphasis (`strip_markdown`, `first_sentence`) — must equal the
`Field.doc` it mirrors (`Field.doc` is one sentence by contract; see
`schema.rs`'s own doc comment on it). The nix prose may say anything it likes
AFTER that first sentence — extra context, the issue reference, the `null`
default's own explanation all stay in `module-common.nix`, just moved past
the first period. Where the two disagreed before this PR (all ten of the
in-scope fields did — see the PR body for the full list), the fix is on the
nix side: the schema `doc` is the one-sentence-by-contract half, so nix's
opening line was rephrased to match it word for word and everything it used
to say up front moved to a following sentence. No `Field.doc` was reworded —
this PR makes no Rust changes at all.

FALSIFYING THE COMPARISON: TWO SELF-TEST LAYERS
--------------------------------------------------
`self_test()` (always runs, first, on every invocation, `nix/lint-glsl.py`
precedent generalised: this script IS the "does the extraction machinery
still work" proof for its OWN primitives, not just the retained mount/places
ones) exercises the parsing primitives — `split_top_level`, `parse_kind`,
`parse_field_literal`, the nix option-tree walker, `first_sentence` — against
small hand-written fixtures built to disagree, the same reasoning
`lint-bind-pins.py`'s header gives for its own fixtures: a clean tree proves
nothing about whether the code that WOULD catch a dirty one still works.
Exit 2 (not 1) if any fixture fails or raises — "the scan itself is
untrustworthy", never confused with "the scan found real drift" (#1270).

`mutation_self_test()` is the OTHER kind of proof, the one ask #5 and
`lint-glsl.py --self-test` (#1325) both ask for: not a parsing primitive in
isolation, but the whole real comparison, on a real (mutated) copy of
`nix/module-common.nix`, against the REAL (unmutated) Rust schema. Four
cases, one per rule class ask #5 names: an enum value dropped
(`core-leds.style` loses `"oled"`), a bound moved (`agents.poll_seconds`'s
upper bound 3600 → 3601), a description's first sentence changed
(`core-leds.fill`), and a `Kind::Map` sub-option removed
(`agents.display.project`). Each mutates an in-memory COPY of the real file
text (`mutate_drop_enum_value`/`mutate_move_bound`/`mutate_first_sentence`/
`mutate_remove_leaf` — never touches disk, the same "no fixture file ships"
reasoning `lint-glsl.py`'s own `self_test()` gives) and asserts the real
`compare()` reports a mismatch naming the family, the dotted path, both
spellings and both files — which every real mismatch message in `compare()`
already does unconditionally, not just in this test.

Both layers run on EVERY invocation, including the plain
`python3 nix/lint-config-vocab.py` the `config-vocab` `runCommand` already
calls (`flake.nix:556-562`, untouched by this PR) — so "run the self-test
before the real scan" (ask #5) needs no second line added there, unlike
`lint-glsl.py`'s two-line `runCommand`. `--self-test` runs ONLY the two
self-test layers (skips the real repo scan) and prints each case's verdict,
for a human or a gate that wants that half in isolation.

FALSIFYING THE PARSER ITSELF: `grep -c 'path:'`
--------------------------------------------------
Ask's own falsification step: `grep -c 'path:'` over each of the four schema
files should equal the parsed leaf count per family (this script prints the
parsed count; the grep cross-check is manual, reported in the PR body,
because two of the four files have a KNOWN, EXPLAINED reason the raw grep
count differs from the true leaf count, and baking a "except when…" carve-out
into the running check would just be a second thing that could rot unnoticed):

  - core-leds: grep 4, parsed 4 (agree)
  - workspaces: grep 8, parsed 8 (agree)
  - agents: grep 7, parsed 6 — `crates/hytte-plugin-agents/src/config.rs`'s
    test module has `use std::path::{Path, PathBuf};`, one more `path:`-
    shaped substring (`std::path::`) that is not a `Field` literal at all.
  - stats: grep 9, parsed 16 — `grep -c 'path:'` counts the EIGHT `path:`
    keys written ONCE in the `card_fields!` template, plus ONE more —
    `crates/hytte-plugin-stats/src/config.rs`'s own test module has
    `use std::path::PathBuf;`, the same `std::path::`-shaped false hit
    `agents` has above. The template's eight expand over TWO table names
    (`"sidebar", "bar"`), so the true leaf count is 8 × 2 = 16, matching the
    file's own "sixteen leaves" doc comment. This is exactly the case the
    ask's own item 1 flags ("the stats family is `card_fields!(…)`-generated
    — see item 3") as needing a decision rather than a blind `grep -c`
    equality.

WHY THIS IS A NIX LINT AND NOT A `cargo test`, AND WHY A HAND-ROLLED SCAN AND
NOT A NIX-EVAL ROUND-TRIP
-------------------------------------------------------------------------------
Unchanged from before #1375 — `nix/package.nix`'s crane source filter keeps
only `.rs`/`.toml`/`Cargo.lock`/CSS/shader files, so a sandboxed `cargo test`
has no `nix/module-common.nix` to read at all (the `include_str!`-of-`assets/`
trap CLAUDE.md documents, reached here via `std::fs::read_to_string`), and
evaluating the nix module for real would need a full `evalModules` call to
read back a handful of literals, plus it could not read the Rust side at all.
A `pkgs.runCommand` reading the real repository tree (`bind-pins`/`glsl`
precedent) needs no compile and reds in seconds.

Run it by hand from the repo root with:

    nix shell nixpkgs#python3 --command python3 nix/lint-config-vocab.py
    nix shell nixpkgs#python3 --command python3 nix/lint-config-vocab.py --self-test

The `nix shell` is not optional: **`python3` is deliberately not on the
devShell PATH** (see `nix/lint-bind-pins.py`'s own header), so a bare
`python3 nix/lint-config-vocab.py` is `command not found`.

USAGE
-----
    python3 nix/lint-config-vocab.py               # self-test, then the real scan
    python3 nix/lint-config-vocab.py --self-test    # self-test only

Exits 0 when everything agrees, 1 naming every mismatch found, 2 when the
scan itself is untrustworthy (a source file is missing, an anchor cannot be
found, a `SCHEMA` parsed to zero fields, or either self-test layer disagrees
with its own fixtures).
"""

from __future__ import annotations

import os
import re
import sys
from dataclasses import dataclass, field as dc_field

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

CORE_LEDS_FAMILY_RS = os.path.join(
    REPO_ROOT, "crates", "hytte-config-families", "src", "core_leds.rs"
)
WORKSPACES_FAMILY_RS = os.path.join(
    REPO_ROOT, "crates", "hytte-config-families", "src", "workspaces.rs"
)
AGENTS_RS = os.path.join(REPO_ROOT, "crates", "hytte-plugin-agents", "src", "config.rs")
STATS_RS = os.path.join(REPO_ROOT, "crates", "hytte-plugin-stats", "src", "config.rs")
MANIFEST_RS = os.path.join(REPO_ROOT, "crates", "hytte-plugin-proto", "src", "manifest.rs")
PLACES_RS = os.path.join(REPO_ROOT, "crates", "hytte-config", "src", "places.rs")
MODULE_COMMON_NIX = os.path.join(REPO_ROOT, "nix", "module-common.nix")

# The nix anchor for each family's own `config.<family> = lib.mkOption { … };`
# block, for the families that have one at all — see "FAMILIES WITH NO NIX
# SURFACE" above for `stats`/`workspaces`, deliberately absent here.
FAMILY_NIX_ANCHORS = {
    "core-leds": "config.core-leds = lib.mkOption {",
    "agents": "config.agents = lib.mkOption {",
}


# ── Generic text primitives (bracket/paren-matching, string-aware splitting) ─


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


def split_top_level(text: str, sep: str = ",") -> list[str]:
    """`text` split on `sep`, but only where `sep` sits outside every string
    literal and every `{}`/`[]`/`()` nesting level — so a `Field`'s `doc`
    string containing a comma, or a `Kind::Choice`'s own `options: &[…]`
    list, cannot be mistaken for the boundary between two sibling items. One
    tokenizer for every "split this struct-literal body into its comma-
    separated pieces" job in this script — `Field { … }` bodies, `Kind::Int`/
    `Choice`/`Text`/`Color` bodies, and a `&[Field { … }, …]` array body all
    go through this, rather than each getting its own regex.

    A closing `"` is required to end a string that was opened, so an
    unterminated literal raises (via `IndexError` reaching the caller as a
    generic exception, caught by `self_test`'s own harness) rather than
    silently consuming the rest of the file — this script never sees
    attacker-controlled input, only its own repository's source, so a loud
    failure here is preferable to a defensive one that could hide a real
    parse bug.
    """
    parts: list[str] = []
    depth = 0
    current: list[str] = []
    i = 0
    n = len(text)
    while i < n:
        c = text[i]
        if c == '"':
            current.append(c)
            i += 1
            while i < n and text[i] != '"':
                if text[i] == "\\" and i + 1 < n:
                    current.append(text[i])
                    current.append(text[i + 1])
                    i += 2
                    continue
                current.append(text[i])
                i += 1
            if i < n:
                current.append(text[i])
                i += 1
            continue
        if c in "{[(":
            depth += 1
            current.append(c)
            i += 1
            continue
        if c in "}])":
            depth -= 1
            current.append(c)
            i += 1
            continue
        if c == sep and depth == 0:
            parts.append("".join(current))
            current = []
            i += 1
            continue
        current.append(c)
        i += 1
    tail = "".join(current).strip()
    if tail:
        parts.append(tail)
    return [p.strip() for p in parts if p.strip()]


def strip_rust_comments(src: str) -> str:
    """`src` with every `//` line comment and `/* … */` block comment blanked
    out, leaving every string literal — plain `"…"` and raw `r#"…"#`/
    `r##"…"##`/… — untouched. `split_top_level`/`kv_segments` walk EVERY
    top-level `,`/`:` character in a `Field { … }`/`Kind::… { … }` body, and
    a doc comment sitting inside one of those bodies is not exempt just
    because it starts with `//` — `core-leds.style`'s own comment
    ("`DisplayStyle::ALL`, in `name()`'s spelling and `ALL`'s order.") has a
    bracket-depth-0 comma in it, which desyncs the very next `kv_segments`
    call onto the WRONG key (`kind:` reads as part of the comment's own
    trailing segment instead of a key), so `parse_field_literal` raises "has
    no `kind:`" on a perfectly well-formed `Field` — reproducibly, on the
    real, unmutated tree, which is what `mutation_self_test` caught. Applied
    once per file, before any bracket-matching, so nothing downstream needs
    to know comments exist.

    Raw strings need their own delimiter tracking — `DEFAULT_TOML`'s
    `r##"…"##` bodies contain plenty of bare `"` and `//` (TOML's own
    comments), and a tracker that only understood plain `"…"` strings would
    lose its place at the FIRST `"` inside one, corrupting the string/comment
    state for everything after it in the file, including the `SCHEMA` const
    that (in `agents.rs`) comes after `DEFAULT_TOML`. Line/column positions
    are not preserved across a stripped comment (it becomes just the
    trailing newline) — nothing downstream keeps an offset into the
    ORIGINAL file, only into this stripped copy's, so that is fine.
    """
    out: list[str] = []
    i = 0
    n = len(src)
    while i < n:
        c = src[i]
        if c == "r" and i + 1 < n and (src[i + 1] == '"' or src[i + 1] == "#"):
            j = i + 1
            hashes = 0
            while j < n and src[j] == "#":
                hashes += 1
                j += 1
            if j < n and src[j] == '"':
                closer = '"' + ("#" * hashes)
                end = src.find(closer, j + 1)
                if end < 0:
                    out.append(src[i:])
                    break
                end_full = end + len(closer)
                out.append(src[i:end_full])
                i = end_full
                continue
        if c == '"':
            start = i
            i += 1
            while i < n and src[i] != '"':
                if src[i] == "\\" and i + 1 < n:
                    i += 2
                    continue
                i += 1
            i += 1
            out.append(src[start:i])
            continue
        if c == "/" and i + 1 < n and src[i + 1] == "/":
            nl = src.find("\n", i)
            if nl < 0:
                i = n
            else:
                out.append("\n")
                i = nl + 1
            continue
        if c == "/" and i + 1 < n and src[i + 1] == "*":
            end = src.find("*/", i + 2)
            i = n if end < 0 else end + 2
            continue
        out.append(c)
        i += 1
    return "".join(out)


def value_until_char(src: str, start: int, stop_char: str) -> tuple[str, int]:
    """`(value_text, end_pos)`: the text from `start` up to the first
    `stop_char` that sits at bracket-depth 0 and outside every string —
    including a nix `''…''` indented string, whose `'''`/`''$`/``''\\``
    escapes are honoured so a stray `;` inside one (`agents.socket`'s
    description has several) cannot look like the attribute's own
    terminator. Used for both `type = …;` and `description = ''…'';` — the
    nested submodule a `Kind::Map` renders as puts its OWN `type =`/
    `description =` pairs, each terminated by their own `;`, entirely inside
    the outer one's value (see `leaf_attrs`'s docstring for why this matters
    for `description` specifically), so this only stops at the outermost `;`,
    never one two levels of nesting inside HELD the outer's own bracket depth
    at 0 the whole time it should not have.
    """
    depth = 0
    i = start
    n = len(src)
    while i < n:
        c = src[i]
        if c == '"':
            i += 1
            while i < n and src[i] != '"':
                if src[i] == "\\":
                    i += 2
                    continue
                i += 1
            i += 1
            continue
        if c == "'" and src[i : i + 2] == "''":
            i += 2
            while True:
                j = src.find("''", i)
                if j < 0:
                    raise LookupError("unterminated `''` string while scanning a value")
                nxt = src[j + 2 : j + 3]
                if nxt in ("'", "$", "\\"):
                    i = j + 3
                    continue
                i = j + 2
                break
            continue
        if c in "{[(":
            depth += 1
        elif c in "}])":
            depth -= 1
        elif c == stop_char and depth == 0:
            return src[start:i], i + 1
        i += 1
    raise LookupError(f"no terminating {stop_char!r} found while scanning a value")


# ── Rust struct-literal primitives ───────────────────────────────────────────


def brace_body(expr: str) -> str:
    """The text strictly between the first `{` and its matching, LAST `}` of
    `expr` — `expr` must be exactly one brace-delimited unit with nothing
    trailing, which every `Kind::Int`/`Choice`/`Text`/`Color { … }` and every
    `Field { … }` this script parses already is by construction (they are
    `kind:`/array-element values `split_top_level` has already isolated)."""
    s = expr.rstrip()
    if not s.endswith("}"):
        raise LookupError(f"expected a `{{ … }}`-terminated expression, got {expr!r}")
    i = s.index("{")
    j = match_delim(s, i, "{", "}")
    if j != len(s):
        raise LookupError(f"trailing content after a `{{ … }}` block in {expr!r}")
    return s[i + 1 : j - 1]


def paren_body(expr: str) -> str:
    """As `brace_body`, for a `Name( … )`-shaped expression — `Kind::List(…)`/
    `Kind::Map(…)`."""
    s = expr.rstrip()
    if not s.endswith(")"):
        raise LookupError(f"expected a `(…)`-terminated expression, got {expr!r}")
    i = s.index("(")
    j = match_delim(s, i, "(", ")")
    if j != len(s):
        raise LookupError(f"trailing content after a `(…)` block in {expr!r}")
    return s[i + 1 : j - 1]


def kv_segments(body: str) -> dict[str, str]:
    """`body` (a struct literal's inner text) split into `{key: raw_value}`,
    one entry per top-level `key: value` segment. A segment with no top-level
    `:` (should not happen for well-formed input) is silently skipped rather
    than raising — the same posture `option_leaves` already takes for a
    non-matching line, because the caller is the one who knows which keys it
    actually needs and will raise its own, more specific `LookupError` when
    one is missing."""
    out: dict[str, str] = {}
    for seg in split_top_level(body, ","):
        key, sep, val = seg.partition(":")
        if not sep:
            continue
        out[key.strip()] = val.strip()
    return out


def unescape_rust_str(s: str) -> str:
    table = {'"': '"', "n": "\n", "t": "\t", "\\": "\\"}
    return re.sub(r"\\(.)", lambda m: table.get(m.group(1), m.group(1)), s)


def parse_str_literal(raw: str) -> str:
    m = re.fullmatch(r'"((?:\\.|[^"\\])*)"', raw.strip(), re.S)
    if not m:
        raise LookupError(f"expected a Rust string literal, got {raw!r}")
    return unescape_rust_str(m.group(1))


def parse_str_array(raw: str) -> list[str]:
    """`&["a", "b"]` / `&[]` → `["a", "b"]` / `[]` — `Kind::Choice`/`Color`'s
    `options`, `Kind::Int`'s `also`."""
    s = raw.strip()
    if s.startswith("&"):
        s = s[1:].strip()
    if not (s.startswith("[") and s.endswith("]")):
        raise LookupError(f"expected a `[…]` string array, got {raw!r}")
    return [parse_str_literal(item) for item in split_top_level(s[1:-1], ",")]


def find_const_array(src: str, name: str) -> str | None:
    """The inner body of `const NAME: &[…] = &[ … ];` (or `pub const`, or
    `&'static […]`) — the type annotation is never parsed, only skipped past:
    this finds `const NAME`, then the first `=`, then the first `[` after
    that `=`, and bracket-matches from there. Returns `None`, not raises, so
    a caller resolving several possible identifiers can try each in turn
    (not needed today — every reference in the four schema files resolves —
    but `RustCtx.array` is what turns a miss into the actual `LookupError`,
    named for the identifier that was being looked up rather than the regex
    that failed to match it)."""
    m = re.search(rf"\bconst {re.escape(name)}\s*:", src)
    if not m:
        return None
    eq = src.find("=", m.end())
    if eq < 0:
        return None
    open_i = src.find("[", eq)
    if open_i < 0:
        return None
    close_i = match_delim(src, open_i, "[", "]")
    if close_i < 0:
        return None
    return src[open_i + 1 : close_i - 1]


def find_kind_alias(src: str, name: str) -> str | None:
    """The RHS expression of `const NAME: Kind = <expr>;` — `workspaces`'
    `APP` (`Kind::Map(APP_FIELDS)`) and `stats`' `POLL_SECONDS`
    (`Kind::Int { … }`), the two places a `List`'s element type or a
    `Field`'s `kind:` is a bare identifier rather than an inline `Kind::…`."""
    m = re.search(rf"\bconst {re.escape(name)}\s*:\s*Kind\s*=\s*", src)
    if not m:
        return None
    value, _ = value_until_char(src, m.end(), ";")
    return value.strip()


def find_int_const(src: str, name: str) -> int | None:
    """`pub const NAME: u64 = 3600;` → `3600` — what `Kind::Int`'s `min`/`max`
    resolve a bare `MIN_POLL_SECONDS`/`MAX_POLL_SECONDS.cast_signed()` against
    (`agents.poll_seconds`, `stats`' `POLL_SECONDS` alias), rather than a
    second pair of integer literals that could drift from the constants the
    parser itself is bounded by."""
    m = re.search(rf"\bconst {re.escape(name)}\s*:\s*[A-Za-z0-9_]+\s*=\s*(-?\d+)\s*;", src)
    if not m:
        return None
    return int(m.group(1))


class RustCtx:
    """Resolves an identifier reference inside one Rust source file — a
    sibling `const NAME: &[Field] = &[ … ];` array, a sibling
    `const NAME: Kind = <expr>;` alias, or a sibling
    `pub const NAME: u64 = …;` integer — the three ways the four schema files
    point at "defined elsewhere in this same file" instead of writing
    everything inline. `where_` names the file in every error message, so a
    `LookupError` says which of the four schemas broke without the caller
    having to thread that through every parse function by hand."""

    def __init__(self, src: str, where_: str) -> None:
        self.src = src
        self.where_ = where_
        self._arrays: dict[str, str] = {}
        self._aliases: dict[str, str] = {}
        self._ints: dict[str, int] = {}

    def array(self, name: str) -> str:
        if name not in self._arrays:
            body = find_const_array(self.src, name)
            if body is None:
                raise LookupError(f"const array `{name}` not found in {self.where_}")
            self._arrays[name] = body
        return self._arrays[name]

    def alias(self, name: str) -> str:
        if name not in self._aliases:
            expr = find_kind_alias(self.src, name)
            if expr is None:
                raise LookupError(f"const Kind alias `{name}` not found in {self.where_}")
            self._aliases[name] = expr
        return self._aliases[name]

    def int_const(self, name: str) -> int:
        if name not in self._ints:
            v = find_int_const(self.src, name)
            if v is None:
                raise LookupError(f"const integer `{name}` not found in {self.where_}")
            self._ints[name] = v
        return self._ints[name]


def parse_int_value(raw: str, ctx: RustCtx) -> int:
    """A `Kind::Int`'s `min`/`max`: a plain integer literal (`core-leds.rows`),
    or `CONST_NAME` / `CONST_NAME.cast_signed()` resolved through `ctx`
    (`agents.poll_seconds`, `stats`' `POLL_SECONDS` alias)."""
    s = raw.strip()
    if re.fullmatch(r"-?\d+", s):
        return int(s)
    m = re.fullmatch(r"([A-Za-z_][A-Za-z0-9_]*)(?:\.cast_signed\(\))?", s)
    if m:
        return ctx.int_const(m.group(1))
    raise LookupError(f"cannot parse an integer bound {raw!r} in {ctx.where_}")


# ── The `Schema`/`Field`/`Kind` data shape, and the parser that builds it ────


@dataclass(frozen=True)
class Kind:
    variant: str  # "Bool" | "Int" | "Choice" | "Text" | "Color" | "List" | "Map"
    min: int = 0
    max: int = 0
    also: tuple[str, ...] = ()
    options: tuple[str, ...] = ()
    blank_ok: bool = False
    elem: Kind | None = None
    fields: tuple[Field, ...] = ()


@dataclass(frozen=True)
class Field:
    path: str
    kind: Kind
    doc: str


def parse_kind(expr: str, ctx: RustCtx) -> Kind:
    """One `Kind::…` expression → a [`Kind`] — the recursive-descent core the
    module docs' "THE GRAMMAR" section describes. `expr` is already the
    isolated `kind:` value text (or a `List`/`Map` element expression),
    trimmed, from `split_top_level`/`kv_segments`/`paren_body`, so it always
    starts with `Kind::…`, `&`, or a bare identifier."""
    e = expr.strip()
    if e == "Kind::Bool":
        return Kind("Bool")
    if e.startswith("Kind::Int"):
        kv = kv_segments(brace_body(e))
        also = parse_str_array(kv.get("also", "&[]"))
        return Kind(
            "Int",
            min=parse_int_value(kv["min"], ctx),
            max=parse_int_value(kv["max"], ctx),
            also=tuple(also),
        )
    if e.startswith("Kind::Choice"):
        kv = kv_segments(brace_body(e))
        return Kind("Choice", options=tuple(parse_str_array(kv["options"])))
    if e.startswith("Kind::Text"):
        kv = kv_segments(brace_body(e))
        return Kind("Text", blank_ok=kv["blank_ok"].strip() == "true")
    if e.startswith("Kind::Color"):
        kv = kv_segments(brace_body(e))
        return Kind("Color", options=tuple(parse_str_array(kv["options"])))
    if e.startswith("Kind::List"):
        inner = paren_body(e).strip()
        if inner.startswith("&"):
            inner = inner[1:].strip()
        elem = parse_kind(inner, ctx) if inner.startswith("Kind::") else parse_kind(ctx.alias(inner), ctx)
        return Kind("List", elem=elem)
    if e.startswith("Kind::Map"):
        inner = paren_body(e).strip()
        if inner.startswith("&"):
            inner = inner[1:].strip()
        if inner.startswith("["):
            j = match_delim(inner, 0, "[", "]")
            if j < 0:
                raise LookupError(f"an inline `Kind::Map(&[ … ])` array never closes in {expr!r}")
            body = inner[1 : j - 1]
        else:
            body = ctx.array(inner)
        return Kind("Map", fields=tuple(parse_fields_array_body(body, ctx)))
    ident = e[1:].strip() if e.startswith("&") else e
    if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", ident):
        return parse_kind(ctx.alias(ident), ctx)
    raise LookupError(f"cannot parse a `Kind` expression {expr!r} in {ctx.where_}")


def parse_field_literal(item_text: str, ctx: RustCtx, *, allow_templated_path: bool = False) -> Field:
    """One `Field { path: …, kind: …, doc: … }` literal → a [`Field`].
    `allow_templated_path=True` (only `card_fields!`'s template body uses
    this) also accepts `concat!($table, "…")` as the `path:` value, returned
    as the Python format string `"{table}.…"` for `parse_card_fields_macro`
    to fill in per table name."""
    s = item_text.strip()
    if not s.startswith("Field"):
        raise LookupError(f"expected a `Field {{ … }}` literal in {ctx.where_}, got {s[:60]!r}")
    kv = kv_segments(brace_body(s))
    for key in ("path", "kind", "doc"):
        if key not in kv:
            raise LookupError(f"a `Field` literal in {ctx.where_} has no `{key}:` — {s[:80]!r}")
    path_raw = kv["path"].strip()
    if allow_templated_path:
        m = re.fullmatch(r'concat!\(\$table,\s*"((?:\\.|[^"\\])*)"\)', path_raw, re.S)
        path = "{table}" + unescape_rust_str(m.group(1)) if m else parse_str_literal(path_raw)
    else:
        path = parse_str_literal(path_raw)
    return Field(path=path, kind=parse_kind(kv["kind"], ctx), doc=parse_str_literal(kv["doc"]))


def parse_fields_array_body(body_text: str, ctx: RustCtx) -> list[Field]:
    return [parse_field_literal(item, ctx) for item in split_top_level(body_text, ",")]


def find_schema_kv(src: str, where_: str) -> dict[str, str]:
    m = re.search(r"pub const SCHEMA:\s*Schema\s*=\s*Schema\s*\{", src)
    if not m:
        raise LookupError(f"`pub const SCHEMA: Schema = Schema {{ … }}` not found in {where_}")
    open_i = m.end() - 1
    close_i = match_delim(src, open_i, "{", "}")
    if close_i < 0:
        raise LookupError(f"the `SCHEMA` struct literal in {where_} never closes")
    return kv_segments(src[open_i + 1 : close_i - 1])


def parse_card_fields_macro(src: str, where_: str, args_text: str, ctx: RustCtx) -> list[Field]:
    """`stats`' one deliberate exception — see the module docs' own section on
    it. Parses `macro_rules! card_fields { ($($table:literal),*) => { &[ … ]
    }; }`'s template ONCE (the same `Field`/`Kind` grammar, `path:` allowed to
    be `concat!($table, "…")`), then materialises it for every table name the
    invocation names."""
    tables = [parse_str_literal(a) for a in split_top_level(args_text, ",")]
    if not tables:
        raise LookupError(f"`card_fields!(…)` in {where_} named no tables")
    idx = src.find("macro_rules! card_fields {")
    if idx < 0:
        raise LookupError(f"`macro_rules! card_fields` not found in {where_}")
    arrow = src.find("=>", idx)
    if arrow < 0:
        raise LookupError(f"`card_fields!`'s `=>` arm not found in {where_}")
    body_open = src.find("{", arrow)
    body_close = match_delim(src, body_open, "{", "}")
    if body_close < 0:
        raise LookupError(f"`card_fields!`'s expansion body never closes in {where_}")
    expansion = src[body_open + 1 : body_close - 1]
    arr_open = expansion.find("[")
    if arr_open < 0:
        raise LookupError(f"`card_fields!`'s expansion in {where_} has no `&[ … ]`")
    arr_close = match_delim(expansion, arr_open, "[", "]")
    if arr_close < 0:
        raise LookupError(f"`card_fields!`'s templated array never closes in {where_}")
    arr_body = expansion[arr_open + 1 : arr_close - 1].strip()
    stripped_end = arr_body.rstrip()
    if not (arr_body.startswith("$(") and stripped_end.endswith(")*")):
        raise LookupError(
            f"`card_fields!`'s template in {where_} is not the expected `$( … )*` "
            "repetition shape this parser understands"
        )
    inner = arr_body[2 : stripped_end.rfind(")*")]
    template = [
        parse_field_literal(item, ctx, allow_templated_path=True)
        for item in split_top_level(inner, ",")
    ]
    if not template:
        raise LookupError(f"`card_fields!`'s template in {where_} has no `Field` entries")
    out: list[Field] = []
    for table in tables:
        for f in template:
            if "{table}" not in f.path:
                raise LookupError(
                    f"`card_fields!`'s template field {f.path!r} in {where_} is not "
                    "templated on `$table` (`concat!($table, \"…\")`) — this parser only "
                    "understands the one shape the macro currently uses"
                )
            out.append(Field(path=f.path.format(table=table), kind=f.kind, doc=f.doc))
    return out


def parse_schema_fields(src: str, where_: str) -> list[Field]:
    """`pub const SCHEMA: Schema = Schema { family: …, fields: <expr> };`'s
    `<expr>` → the family's `list[Field]` — an identifier (`core-leds`/
    `workspaces`' shape), an inline `&[ Field { … }, … ]` (`agents`' shape),
    or a `card_fields!(…)` invocation (`stats`' shape, see
    `parse_card_fields_macro`)."""
    kv = find_schema_kv(src, where_)
    if "fields" not in kv:
        raise LookupError(f"`SCHEMA` in {where_} has no `fields:` key")
    fields_expr = kv["fields"].strip()
    ctx = RustCtx(src, where_)
    m = re.fullmatch(r"card_fields!\((.*)\)", fields_expr, re.S)
    if m:
        return parse_card_fields_macro(src, where_, m.group(1), ctx)
    if fields_expr.startswith("&["):
        inner = fields_expr[1:].strip()
        j = match_delim(inner, 0, "[", "]")
        if j < 0:
            raise LookupError(f"the inline `fields: &[ … ]` array in {where_} never closes")
        return parse_fields_array_body(inner[1 : j - 1], ctx)
    return parse_fields_array_body(ctx.array(fields_expr), ctx)


def count_leaves(fields: list[Field]) -> int:
    """Every `Field` this schema parses to, at every nesting level — what a
    `grep -c 'path:'` over the same file counts too (see the module docs'
    falsification section), since every `Field` literal has exactly one
    `path:` key regardless of how deep inside a `Map`/`List(Map)` it sits."""
    total = 0
    for f in fields:
        total += 1
        if f.kind.variant == "Map":
            total += count_leaves(list(f.kind.fields))
        elif f.kind.variant == "List" and f.kind.elem is not None and f.kind.elem.variant == "Map":
            total += count_leaves(list(f.kind.elem.fields))
    return total


# ── The nix option-tree parser ───────────────────────────────────────────────


def find_block_span(src: str, anchor: str) -> tuple[int, int]:
    """`(open_i, close_i)`: `open_i` is the index of the `{` the `anchor`
    string itself ends with, `close_i` is just past its matching `}`."""
    start = src.find(anchor)
    if start < 0:
        raise LookupError(f"anchor {anchor!r} not found")
    open_i = start + len(anchor) - 1
    if src[open_i] != "{":
        raise LookupError(f"anchor {anchor!r} does not end at an opening brace")
    close_i = match_delim(src, open_i, "{", "}")
    if close_i < 0:
        raise LookupError(f"the block opened by anchor {anchor!r} never closes")
    return open_i, close_i


def option_block(src: str, anchor: str) -> str:
    """The brace-matched body of the option declaration named by `anchor`,
    INCLUDING its own outer braces (`anchor` ends at the opening one) —
    unchanged from before #1375, still used by the retained `places` arm."""
    open_i, close_i = find_block_span(src, anchor)
    return src[open_i:close_i]


def option_leaves(body: str) -> list[str]:
    """Every `<name> = lib.mkOption {` declared inside `body`, in source
    order — unchanged from before #1375, still used by the retained `places`
    arm (`places_option_levels`), which needs only the flat NAME list at one
    level, not the richer per-option [`NixOption`] tree `parse_options_level`
    builds for the two schema-backed families below."""
    return re.findall(r"^\s*(\w+) = lib\.mkOption \{", body, re.M)


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


LEAF_RE = re.compile(r"(\w+)\s*=\s*lib\.mkOption\s*\{")


@dataclass
class NixOption:
    name: str
    type_expr: str
    description_raw: str | None
    sub_options: dict[str, "NixOption"] | None


def find_attr(body: str, name: str, start: int = 0) -> tuple[str, int, int] | None:
    """`(raw_value_text, match_start, end_pos)` for the FIRST `name =
    <value>;` found at or after `start` in `body`, or `None`. `match_start`
    is where the `name` token itself begins — not `start` — so a caller can
    test whether this particular match fell INSIDE another attribute's own
    value span (`leaf_attrs` does, for exactly the reason below); `end_pos`
    is where the caller should resume searching for the NEXT attribute."""
    m = re.search(rf"(?m)^\s*{re.escape(name)}\s*=\s*", body[start:])
    if not m:
        return None
    match_start = start + m.start()
    value, end = value_until_char(body, start + m.end(), ";")
    return value, match_start, end


def leaf_attrs(leaf_body: str) -> tuple[str, str | None]:
    """`(type_expr, description_raw)` for one `lib.mkOption { … }`'s own
    body. A `Kind::Map` option's own nested sub-options each carry a
    `type =`/`description =` pair too, entirely inside the outer option's
    `type` value, and a naive "first match anywhere in the body" for
    `description` would find a NESTED one instead of the outer leaf's own —
    so every `description` match is checked against `type`'s own span
    (`[type_start, type_end)`) and skipped if it falls inside, however many
    there are and wherever `type` itself sits. This does NOT assume `type`
    is the leaf's first attribute (nix-fmt's convention, but not one this
    parser should be brittle against — #1378 review LOW 6: a `type` written
    last used to make this function report "no description" on a leaf that
    plainly has one, because the old skip-ahead started searching only
    AFTER `type`'s span, which is empty of anything when `type` comes last)."""
    type_res = find_attr(leaf_body, "type", 0)
    if type_res is None:
        raise LookupError("a `lib.mkOption { … }` leaf has no `type =` attribute")
    type_expr, type_start, type_end = type_res

    description_raw = None
    pos = 0
    while True:
        desc_res = find_attr(leaf_body, "description", pos)
        if desc_res is None:
            break
        desc_value, desc_start, desc_end = desc_res
        if type_start <= desc_start < type_end:
            pos = desc_end
            continue
        v = desc_value.strip()
        if not (v.startswith("''") and v.endswith("''") and len(v) >= 4):
            raise LookupError(f"a `description =` value is not a `''…''` string: {v[:60]!r}")
        description_raw = v[2:-2]
        break
    return type_expr.strip(), description_raw


def leaf_sub_options(type_expr: str) -> dict[str, NixOption] | None:
    """The nested `options = { … }` a `Kind::Map`/`Kind::List(Map)`-shaped
    option's `type` renders as an `attrsOf`/`listOf (submodule { options =
    … })` — or `None` if `type_expr` has no such block (every scalar kind)."""
    m = re.search(r"\boptions\s*=\s*\{", type_expr)
    if not m:
        return None
    open_i = m.end() - 1
    close_i = match_delim(type_expr, open_i, "{", "}")
    if close_i < 0:
        raise LookupError("a nested `options = { … }` block never closes")
    return parse_options_level(type_expr[open_i + 1 : close_i - 1])


def parse_options_level(body: str) -> dict[str, NixOption]:
    """Every `<name> = lib.mkOption { … };` declared DIRECTLY inside `body` (an
    `options = { … }` block's own inner text) — a sequential scan, not a
    global regex sweep, is what keeps this to direct children only: each
    match's own body is brace-matched and the cursor jumps PAST it before
    searching for the next, so a match nested two levels down (inside a
    `Kind::Map` leaf's own `type`) is never independently rediscovered as a
    sibling of the leaf that contains it — it comes back through that leaf's
    own `sub_options` instead."""
    out: dict[str, NixOption] = {}
    pos = 0
    n = len(body)
    while pos < n:
        m = LEAF_RE.search(body, pos)
        if not m:
            break
        name = m.group(1)
        open_i = m.end() - 1
        close_i = match_delim(body, open_i, "{", "}")
        if close_i < 0:
            raise LookupError(f"`{name} = lib.mkOption {{` never closes")
        leaf_body = body[open_i + 1 : close_i - 1]
        type_expr, description_raw = leaf_attrs(leaf_body)
        out[name] = NixOption(name, type_expr, description_raw, leaf_sub_options(type_expr))
        pos = close_i
    return out


def parse_family_options(nix_src: str, anchor: str) -> dict[str, NixOption]:
    """`config.<family> = lib.mkOption { type = lib.types.submodule { options
    = { … }; }; … };`'s own `options = { … }` body, parsed into the
    [`NixOption`] tree `compare()` walks alongside the [`Schema`]'s own
    `Field` tree."""
    block = option_block(nix_src, anchor)
    m = re.search(r"\boptions\s*=\s*\{", block)
    if not m:
        raise LookupError(f"no nested `options = {{ … }}` found for anchor {anchor!r}")
    open_i = m.end() - 1
    close_i = match_delim(block, open_i, "{", "}")
    if close_i < 0:
        raise LookupError(f"the `options = {{ … }}` block for anchor {anchor!r} never closes")
    return parse_options_level(block[open_i + 1 : close_i - 1])


def unexpected_nix_surface(
    nix_src: str, family_names: list[str], known_anchors: dict[str, str]
) -> list[str]:
    """Every name in `family_names` that is NOT a key of `known_anchors`
    (i.e. a family `run_real_scan` is about to call "skipped: no nix
    surface") but for which `nix_src` actually contains a
    `config.<family> = lib.mkOption {` block anyway — the exact anchor
    shape `known_anchors`' own values spell (#1378 review, MEDIUM 1).

    `FAMILY_NIX_ANCHORS` is a hardcoded allowlist: before this function
    existed, a family gaining a REAL nix block (someone starting #1374,
    or a typo'd anticipatory stub) was invisible to the scan — it still
    was not in the allowlist, so `compare()` was never called on it, and
    the success line printed "skipped: no nix surface" and exited 0 over
    a block whose vocabulary nothing was checking. Proven by the
    reviewer: adding a real `config.workspaces` block with a WRONG leaf
    set to `nix/module-common.nix` left the scan green.

    A plain substring search, not a brace-matched one: this only needs to
    know a block with this anchor EXISTS, not what it says — the caller's
    job on a hit is to refuse to run, in the same "the scan itself is
    untrustworthy" register `self_test`'s own failures use, not to
    attempt a comparison `FAMILY_NIX_ANCHORS` was never taught to run."""
    return [name for name in family_names if name not in known_anchors and f"config.{name} = lib.mkOption {{" in nix_src]


# ── Doc comparison: nix `description`'s first sentence vs. schema `doc` ─────

_MD_BOLD_RE = re.compile(r"\*\*(.+?)\*\*", re.S)
_MD_ITALIC_RE = re.compile(r"(?<!\*)\*(?!\*)(.+?)(?<!\*)\*(?!\*)", re.S)


def strip_markdown(text: str) -> str:
    """Backticks and `**bold**`/`*italic*` emphasis removed — what the doc
    gate compares UNDER, so nix's prose may keep its own styling (`**absolute**`,
    `` `null` ``) without that styling counting as a difference from the
    schema's plain-text `doc`."""
    text = text.replace("`", "")
    text = _MD_BOLD_RE.sub(r"\1", text)
    text = _MD_ITALIC_RE.sub(r"\1", text)
    return text


def normalize_ws(text: str) -> str:
    return re.sub(r"\s+", " ", text).strip()


_SENTENCE_ABBREVIATIONS = ("e.g.", "i.e.")


def first_sentence(text: str) -> str:
    """Up to the first `.` followed by whitespace or end of string, after
    `strip_markdown` and whitespace normalisation — the exact rule the PR
    brief specifies for the doc gate. Applied to BOTH sides (a schema `doc`
    is one sentence by contract, but running it through this too costs
    nothing and means a doc that ever grew a second sentence by accident
    would compare against only its own first, not silently swallow the nix
    side's remainder into the comparison).

    A period ending `e.g.`/`i.e.` (case-insensitively) is not treated as a
    sentence boundary — those abbreviations end in a period without ending
    the sentence (#1378 review LOW 7). No `Field.doc` in the tree uses
    either today EXCEPT `stats.{sidebar,bar}.gpu`'s ("The GPU half. It hides
    itself when there is no GPU to read." — no `e.g.`/`i.e.` there either,
    but it IS already two sentences, so it is the one doc in the tree this
    matters for the day `stats` gains a nix surface (#1374): without this
    fix `first_sentence` would silently truncate at "half." either way, and
    a doc gate comparing full sentences would need this exemption to have a
    chance at matching a hand-written nix description that also uses one."""
    stripped = strip_markdown(text)
    pos = 0
    while True:
        m = re.search(r"\.(?=\s|$)", stripped[pos:])
        if not m:
            return normalize_ws(stripped)
        end = pos + m.end()
        if stripped[max(0, end - 4) : end].lower() in _SENTENCE_ABBREVIATIONS:
            pos = end
            continue
        return normalize_ws(stripped[:end])


def compact(type_expr: str) -> str:
    return normalize_ws(type_expr)


# ── The comparison ────────────────────────────────────────────────────────


def strip_null_or(type_expr: str) -> str:
    """`type_expr` with exactly one leading `lib.types.nullOr` wrapper
    removed — the nix side's spelling of "no opinion, defer to a lower
    layer" (#1227), allowed on every field regardless of `Kind` (see the
    module docs). The parenthesised form (`lib.types.nullOr ( … )`) is
    bracket-matched with `match_delim`, not a regex, so a Map/List option's
    own nested parens inside the wrapped type can't make this stop early;
    the bare form (`lib.types.nullOr lib.types.bool`, no parens) is
    stripped by the next token instead. Only ONE layer is ever stripped —
    "nothing but `nullOr` is transparent" (#1378 review, MEDIUM 4): a
    double-wrapped or otherwise malformed type is handed back UNCHANGED
    (still starting with `lib.types.nullOr`) for `top_level_shape` to
    reject below, rather than peeled away as if it were a scalar."""
    s = type_expr.strip()
    m = re.match(r"lib\.types\.nullOr\s*", s)
    if not m:
        return s
    rest = s[m.end() :]
    if rest.startswith("("):
        close = match_delim(rest, 0, "(", ")")
        if close < 0:
            return s
        inner = rest[1 : close - 1].strip()
        trailing = rest[close:].strip()
        return inner if not trailing else s
    return rest.strip() if rest.strip() else s


def split_either_arms(core_raw: str) -> tuple[str, str] | None:
    """The two `( … )` arms of a `lib.types.either ( … ) ( … )` expression,
    bracket-matched with `match_delim` — never `(.*?)`, which would cut an
    arm short at its OWN first nested `)` (neither arm this script compares
    has one today, but a regex that only happens to work today is the same
    trap `top_level_shape` exists to close). `None` if `core_raw` (already
    `nullOr`-stripped, NOT yet whitespace-`compact`ed — the offsets this
    walks are the raw file's own) isn't shaped like `either ( … ) ( … )` at
    all, or carries trailing content after the second arm."""
    s = core_raw.strip()
    m = re.match(r"lib\.types\.either\s*", s)
    if not m or not s[m.end() :].lstrip().startswith("("):
        return None
    rest = s[m.end() :].lstrip()
    close1 = match_delim(rest, 0, "(", ")")
    if close1 < 0:
        return None
    arm1 = rest[1 : close1 - 1].strip()
    rest2 = rest[close1:].strip()
    if not rest2.startswith("("):
        return None
    close2 = match_delim(rest2, 0, "(", ")")
    if close2 < 0:
        return None
    arm2 = rest2[1 : close2 - 1].strip()
    if rest2[close2:].strip():
        return None
    return arm1, arm2


def top_level_shape(core: str) -> str:
    """Which nix type constructor `core` — already `nullOr`-stripped and
    `compact()`ed to one line — actually IS: a FULL-STRING match for the
    scalar shapes (`bool`/`str`/`strMatching`/`enum`/`ints.between`), an
    anchored PREFIX for the three combinators (`either`/`listOf`/
    `attrsOf`) whose own arguments the caller re-descends into separately.
    Returns `""` for anything else, which every caller treats as "does not
    match" — never a wildcard pass.

    This replaces the substring checks (`"lib.types.str" in t`, `"enum" in
    t`, …) #1378 review MEDIUM 4 found: `agents.display.label`'s type is
    `nullOr (listOf lib.types.str)`, and `"lib.types.str" in t` is TRUE for
    it too — the substring sits right there inside `listOf`'s own argument
    — so a `Kind::Text` leaf accidentally re-typed as a list of strings
    read as agreeing. A `re.fullmatch`/exact-equality check against the
    WHOLE remaining expression cannot have that hole: `listOf lib.types.str`
    is not equal to, and does not fullmatch, `lib.types.str`."""
    if core == "lib.types.bool":
        return "bool"
    if core == "lib.types.str":
        return "str"
    if re.fullmatch(r'lib\.types\.strMatching\s+".*"', core, re.S):
        return "strMatching"
    if re.fullmatch(r"lib\.types\.enum\s*\[.*\]", core, re.S):
        return "enum"
    if re.fullmatch(r"lib\.types\.ints\.between\s+-?\d+\s+-?\d+", core):
        return "ints.between"
    if re.match(r"lib\.types\.either\s*\(", core):
        return "either"
    if re.match(r"lib\.types\.listOf\s+", core):
        return "listOf"
    if re.match(r"lib\.types\.attrsOf\s*\(", core):
        return "attrsOf"
    return ""


def check_kind_vs_nix_type(kind: Kind, opt: NixOption, rust_file: str) -> list[str]:
    """`opt.type_expr` against what `kind` says the nix type should be — see
    the module docs' comparison table for the full mapping, including why
    `Color` folds into `Text`'s rule and why `Text`/`Color` accept a stricter
    `strMatching` as well as a plain `str`. Every message names both files
    and both spellings so a real mismatch (or a `mutation_self_test` case)
    never has to be traced back through the caller to know what disagreed.

    Works over `core` — `opt.type_expr` with exactly one `nullOr` wrapper
    stripped (`strip_null_or`) and then whitespace-collapsed to one line
    (`compact`) — and classifies its OUTER constructor with
    `top_level_shape` before comparing anything, so a scalar Kind can never
    be satisfied by a substring sitting inside a combinator's own argument
    (#1378 review, MEDIUM 4)."""
    t = opt.type_expr
    core_raw = strip_null_or(t)
    core = compact(core_raw)
    shape = top_level_shape(core)
    errs: list[str] = []

    def wrong_shape(expect: str) -> str:
        return (
            f"nix/module-common.nix's type is `{compact(t)}`, but {rust_file}'s "
            f"`Kind::{kind.variant}` expects `{expect}`"
        )

    if kind.variant == "Bool":
        if shape != "bool":
            errs.append(wrong_shape("lib.types.bool"))
    elif kind.variant == "Int":
        if kind.also:
            if shape != "either":
                errs.append(
                    f"{rust_file}'s `Kind::Int.also` is {list(kind.also)}, so "
                    "nix/module-common.nix's type must be `lib.types.either (ints.between …) "
                    f"(enum […])`, but its type is `{compact(t)}`"
                )
            else:
                arms = split_either_arms(core_raw)
                if arms is None:
                    errs.append(
                        f"nix/module-common.nix's `either` type could not be split into its "
                        f"two `( … )` arms: `{compact(t)}`"
                    )
                else:
                    arm1, arm2 = compact(arms[0]), compact(arms[1])
                    m = re.fullmatch(r"lib\.types\.ints\.between\s+(-?\d+)\s+(-?\d+)", arm1)
                    if not m:
                        errs.append(
                            f"nix/module-common.nix's `either`'s first arm is `{arm1}`, but "
                            f"{rust_file}'s `Kind::Int` expects `ints.between {kind.min} {kind.max}`"
                        )
                    else:
                        lo, hi = int(m.group(1)), int(m.group(2))
                        if (lo, hi) != (kind.min, kind.max):
                            errs.append(
                                f"nix/module-common.nix bounds it {lo}..{hi} (`ints.between {lo} "
                                f"{hi}`), but {rust_file}'s `Kind::Int` says {kind.min}..{kind.max}"
                            )
                    em = re.fullmatch(r"lib\.types\.enum\s*\[(.*)\]", arm2, re.S)
                    got_also = [tok.strip('"') for tok in em.group(1).split() if tok.strip('"')] if em else []
                    if got_also != list(kind.also):
                        errs.append(
                            f"nix/module-common.nix's `either`'s word enum is {got_also}, but "
                            f"{rust_file}'s `Kind::Int.also` is {list(kind.also)}"
                        )
        else:
            if shape != "ints.between":
                errs.append(wrong_shape(f"lib.types.ints.between {kind.min} {kind.max}"))
            else:
                m = re.fullmatch(r"lib\.types\.ints\.between\s+(-?\d+)\s+(-?\d+)", core)
                lo, hi = int(m.group(1)), int(m.group(2))
                if (lo, hi) != (kind.min, kind.max):
                    errs.append(
                        f"nix/module-common.nix bounds it {lo}..{hi} (`ints.between {lo} {hi}`), "
                        f"but {rust_file}'s `Kind::Int` says {kind.min}..{kind.max}"
                    )
    elif kind.variant == "Choice":
        if shape != "enum":
            errs.append(wrong_shape("lib.types.enum [ … ]"))
        else:
            em = re.fullmatch(r"lib\.types\.enum\s*\[(.*)\]", core, re.S)
            got = [tok.strip('"') for tok in em.group(1).split() if tok.strip('"')]
            if got != list(kind.options):
                errs.append(
                    f"nix/module-common.nix's enum is {got}, but {rust_file}'s "
                    f"`Kind::Choice.options` is {list(kind.options)}"
                )
    elif kind.variant in ("Text", "Color"):
        if shape not in ("str", "strMatching"):
            errs.append(
                f"nix/module-common.nix's type is `{compact(t)}`, but {rust_file}'s "
                f"`Kind::{kind.variant}` expects `lib.types.str` (or a stricter "
                '`lib.types.strMatching "…"`)'
            )
    elif kind.variant == "Map":
        if shape != "attrsOf":
            errs.append(wrong_shape("lib.types.attrsOf (lib.types.submodule …)"))
        elif "lib.types.submodule" not in core:
            errs.append(
                f"{rust_file} declares this a `Kind::Map`, but nix/module-common.nix's "
                f"`attrsOf` does not wrap a `lib.types.submodule`: `{compact(t)}`"
            )
        if opt.sub_options is None:
            errs.append(
                f"{rust_file} declares this a `Kind::Map`, but nix/module-common.nix's type "
                "has no nested `options = { … }`"
            )
    elif kind.variant == "List":
        if shape != "listOf":
            errs.append(wrong_shape("lib.types.listOf …"))
        else:
            elem_core = core[len("lib.types.listOf ") :].strip()
            elem_shape = top_level_shape(elem_core)
            if kind.elem is not None and kind.elem.variant == "Map" and opt.sub_options is None:
                errs.append(
                    f"{rust_file}'s `Kind::List` elements are a `Kind::Map`, but "
                    "nix/module-common.nix's `listOf` has no nested `options = { … }`"
                )
            elif kind.elem is not None and kind.elem.variant == "Text" and elem_shape not in ("str", "strMatching"):
                errs.append(
                    f"nix/module-common.nix's type is `{compact(t)}`, but {rust_file}'s "
                    "`Kind::List(Text)` expects a `listOf lib.types.str`"
                )
    return errs


def compare(
    family: str,
    rust_file: str,
    schema_fields: list[Field],
    nix_options: dict[str, NixOption],
    mismatches: list[str],
    prefix: str = "",
) -> None:
    """Schema `Field`s vs. nix [`NixOption`]s, at one nesting level, appending
    to `mismatches` and recursing into every `Kind::Map`/`Kind::List(Map)`
    both sides agree exists. `prefix` is the dotted path already descended
    (`""` at the top, `"display"` inside `agents.display`'s sub-fields, …)."""
    schema_by_path = {f.path: f for f in schema_fields}
    option_path = f"programs.trollshell.config.{family}" + (f".{prefix}" if prefix else "")

    only_rust = sorted(set(schema_by_path) - set(nix_options))
    only_nix = sorted(set(nix_options) - set(schema_by_path))
    for p in only_rust:
        full = f"{prefix}.{p}" if prefix else p
        mismatches.append(
            f"{family}.{full}: field exists in {rust_file}'s schema but `{option_path}` "
            f"(nix/module-common.nix) has no `{p}` option leaf — nix can never set it"
        )
    for p in only_nix:
        full = f"{prefix}.{p}" if prefix else p
        mismatches.append(
            f"{family}.{full}: `{option_path}.{p}` exists in nix/module-common.nix but is "
            f"not a field of {rust_file}'s schema — rendering it would only warn at load time"
        )

    for name in sorted(set(schema_by_path) & set(nix_options)):
        field = schema_by_path[name]
        opt = nix_options[name]
        full = f"{prefix}.{name}" if prefix else name

        for err in check_kind_vs_nix_type(field.kind, opt, rust_file):
            mismatches.append(f"{family}.{full}: {err}")

        want = first_sentence(field.doc)
        if opt.description_raw is None:
            mismatches.append(
                f"{family}.{full}: nix/module-common.nix's `{name}` option has no "
                f"`description = ''…'';` to check against {rust_file}'s schema doc {want!r}"
            )
        else:
            got = first_sentence(opt.description_raw)
            if got != want:
                mismatches.append(
                    f"{family}.{full}: nix/module-common.nix's description's first sentence "
                    f"is {got!r}, but {rust_file}'s schema doc is {want!r}"
                )

        if field.kind.variant == "Map":
            compare(family, rust_file, list(field.kind.fields), opt.sub_options or {}, mismatches, full)
        elif field.kind.variant == "List" and field.kind.elem is not None and field.kind.elem.variant == "Map":
            compare(
                family, rust_file, list(field.kind.elem.fields), opt.sub_options or {}, mismatches, full
            )


# ── Retained unchanged: `plugins.<id>.mount` ↔ `Mount::ALL` (#1161) ─────────


def _enum_all_names(src: str, ty: str, fn: str, where: str) -> list[str]:
    """`<ty>::ALL`'s variants mapped through `fn <fn>(self) -> &'static str`.

    Reads `pub const ALL: [Self; N] = [Self::Vfd, Self::Lcd, …];` (or the
    `[Mount; N] = [Mount::…]` spelling — both appear in the tree) for the
    variant *order*, then that function's match arms for the
    variant -> string mapping, and composes the two. This is exactly what
    `<ty>::ALL.iter().map(|v| v.<fn>())` computes at runtime, so the nix side
    is checked against the same sequence the Rust schema itself would
    resolve.

    Originally two enums went through this — `DisplayStyle::ALL`/`name` in
    `crates/hytte-preem/src/style.rs` and `Mount::ALL`/`wire_name` in
    `crates/hytte-plugin-proto/src/manifest.rs` — kept as one function rather
    than two near-copies because the only differences were the type's
    spelling and the method's name. `DisplayStyle::ALL` no longer goes
    through this: #1375 retired that scrape (core-leds' `style` enum now
    comes from its `Schema` instead, see the module docs), so
    `mount_wire_names` below is the only remaining caller — restored
    unchanged rather than collapsed into `mount_wire_names` directly, since a
    second enum reaching this shape again is exactly the case the original
    "a second copy would be a second thing to fix" reasoning was for.
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


def mount_wire_names(src: str) -> list[str]:
    """`Mount`'s wire names, in `ALL`'s order (#1161, #1260 review F3).

    The vocabulary `programs.trollshell.plugins.<id>.mount`'s `types.enum`
    hand-mirrors, and the one the SDK matches `HYTTE_PLUGIN_MOUNT` against at
    plugin startup — `Mount::from_wire_name` is `wire_name`'s exact inverse,
    so a value outside this list is a launch failure rather than a card in
    the wrong place.
    """
    return _enum_all_names(src, "Mount", "wire_name", "crates/hytte-plugin-proto/src/manifest.rs")


# ── Retained unchanged: `places` (#1339 item 2 — not a `Schema` family) ────

# `place` and `departures` are SIBLING leaves of `config.places`, not one
# other — `place` is a `listOf (submodule { … })` whose own options are
# `PlaceCfg`'s fields, `departures` is a plain `submodule` whose own options
# are `DeparturesCfg`'s — so `places_option_levels` reads each independently,
# with no block to lift out first. There is deliberately no third entry for
# `config.places` itself: its own two leaves (`place`, `departures`) are not
# the serde fields of any single Rust struct — `PlaceCfg` and `DeparturesCfg`
# are two separate parse structs, both private (see the module docs' "TWO
# ARMS THAT STAY EXACTLY AS THEY WERE"), so `struct_serde_fields` is called
# for these two with `private=True`.
PLACES_STRUCT_LEVELS = (
    ("place = lib.mkOption {", "PlaceCfg"),
    ("departures = lib.mkOption {", "DeparturesCfg"),
)

# The (struct, file) pairs `struct_serde_fields` may read without requiring a
# `pub` on the declaration or on its fields — see the module docs' "TWO ARMS
# THAT STAY EXACTLY AS THEY WERE" for why these two stay private rather than
# being made `pub` for this script's convenience. Keyed by struct name
# (checked by `struct_serde_fields` itself, so no call site needs to opt in
# by hand); the file is carried alongside for `main()` to read from and so
# this table stays the one place that says both "which structs" and "from
# where". Anything not named here still requires `pub struct` with `pub`
# fields, the stricter default every other family is held to.
PRIVATE_STRUCTS = {struct: PLACES_RS for _, struct in PLACES_STRUCT_LEVELS}


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
    """The serde-visible field names of `struct <struct>`, in source order.

    Requires a `pub struct` with `pub` fields UNLESS `struct` is one of
    `PRIVATE_STRUCTS`' keys (`places.rs`'s `PlaceCfg`/`DeparturesCfg`, #1339
    item 2) — those are read without either `pub`, since making a
    file-schema struct public just to satisfy this scan would widen a
    published API for the lint's convenience. The distinction is by struct
    NAME, looked up here, so no call site has to remember to ask for it and
    every family this table doesn't name keeps the stricter default.

    Raises rather than guessing if the struct (or its container attributes)
    uses a serde spelling this scan cannot follow — `rename`, `rename_all` and
    `flatten` all make the wire name something other than the Rust field name,
    and a scan that quietly ignored them would compare the nix side against
    names that never appear in the TOML. An untrustworthy verdict is worth
    exit 2, not a green.
    """
    private = struct in PRIVATE_STRUCTS
    struct_kw = "struct" if private else "pub struct"
    m = re.search(rf"{struct_kw} {struct}\b[^{{]*\{{", src)
    if not m:
        raise LookupError(f"`{struct_kw} {struct}` not found")
    # Container attributes sit between the doc comment and the struct keyword;
    # 500 characters back covers the derive list and any `#[serde(...)]` line.
    head = src[max(0, m.start() - 500) : m.start()]
    if _serde_has(head, r"rename_all"):
        raise LookupError(f"`{struct}` carries a serde `rename_all` this scan cannot follow")
    body_start = m.end() - 1
    body_end = match_delim(src, body_start, "{", "}")
    if body_end < 0:
        raise LookupError(f"`{struct_kw} {struct}`'s body brace never closes")
    body = src[body_start:body_end]
    if _serde_has(body, r"rename\s*="):
        raise LookupError(f"`{struct}` uses serde `rename`, which this scan cannot follow")
    if _serde_has(body, r"\bflatten\b"):
        raise LookupError(f"`{struct}` uses serde `flatten`, which this scan cannot follow")
    if private:
        fields = re.findall(r"^\s*(\w+):", body, re.M)
    else:
        fields = re.findall(r"pub(?:\([^)]*\))?\s+(\w+):", body)
    if not fields:
        raise LookupError(f"`{struct_kw} {struct}` has no fields")
    return fields


def places_option_levels(nix_src: str) -> dict[str, list[str]]:
    """`config.places`' option leaves, one list per Rust struct (#1339 item 2).

    `place` and `departures` are SIBLING leaves of `config.places` —
    `PlaceCfg`'s fields live inside the `listOf (submodule { … })` under
    `place`, `DeparturesCfg`'s inside the plain `submodule` under
    `departures` — neither block sits inside the other, so each is read
    independently with no "lift the nested block out first" step (the shape
    the old `agents_option_levels` needed for `agents.display`'s sub-fields,
    before #1375 retired that arm's own hand-mirror in favour of the schema
    path).
    """
    return {struct: option_leaves(option_block(nix_src, anchor)) for anchor, struct in PLACES_STRUCT_LEVELS}


def compare_places(nix_src: str, places_src: str, mismatches: list[str]) -> dict[str, list[str]]:
    """`places`' own (unchanged) comparison — `place`/`departures`' option
    leaves vs. `PlaceCfg`/`DeparturesCfg`'s serde fields, set-compared both
    directions. Kept as its own function (rather than folded into
    `compare()`, which walks a [`Schema`] `places` does not have) precisely
    because #1375 leaves this arm untouched."""
    nix_levels = places_option_levels(nix_src)
    rust_levels = {struct: struct_serde_fields(places_src, struct) for _, struct in PLACES_STRUCT_LEVELS}
    for _, struct in PLACES_STRUCT_LEVELS:
        nix_keys = set(nix_levels[struct])
        rust_keys = set(rust_levels[struct])
        only_rust = sorted(rust_keys - nix_keys)
        only_nix = sorted(nix_keys - rust_keys)
        if only_rust:
            mismatches.append(
                f"{struct}: field(s) {only_rust} exist in {PLACES_RS} but have no "
                "`programs.trollshell.config.places` option leaf — nix can never set them"
            )
        if only_nix:
            mismatches.append(
                f"{struct}: option leaf(s) {only_nix} exist under "
                f"`programs.trollshell.config.places` but are not fields of `{struct}` — "
                "rendering them would only produce an unknown-key warning at load time"
            )
    return rust_levels


# ── `self_test()`: the parsing primitives, against fixtures built to disagree

def self_test() -> list[str]:
    failures: list[str] = []

    # split_top_level: nested brackets, a string containing the separator and
    # brace-like characters, a trailing separator.
    got = split_top_level('a, "b, {not a nest}", [c, d], e', ",")
    if got != ['a', '"b, {not a nest}"', "[c, d]", "e"]:
        failures.append(f"split_top_level: nested/quoted content mis-split, got {got}")
    if split_top_level("a, b,", ",") != ["a", "b"]:
        failures.append("split_top_level: a trailing separator produced an extra empty item")

    # strip_rust_comments: a `//` comment with a bracket-depth-0 comma inside
    # a `Field { … }` body must not desync `kv_segments`'s key/value split —
    # this is the exact shape of the real bug `core-leds.rs`'s `style` field
    # comment tripped (`mutation_self_test` on the real tree is what caught
    # it originally, before this fixture existed): a comment ending in a
    # comma, immediately followed by the next real key.
    commented = (
        'Field {\n'
        '    path: "style",\n'
        "    // has a, comma and a: colon in the comment\n"
        "    kind: Kind::Bool,\n"
        '    doc: "d",\n'
        "}"
    )
    stripped = strip_rust_comments(commented)
    if "//" in stripped or "comma" in stripped:
        failures.append(f"strip_rust_comments: a `//` line comment survived stripping: {stripped!r}")
    kv = kv_segments(brace_body(stripped))
    if kv.get("kind", "").strip() != "Kind::Bool":
        failures.append(
            f"strip_rust_comments: a stripped comment still desynced kv_segments, got {kv}"
        )
    # A raw string's own `"` and `//` (TOML comments, URLs) must survive
    # untouched, and comment-stripping past the raw string must not lose its
    # place — the `DEFAULT_TOML` reason `strip_rust_comments`'s own docstring
    # gives for tracking `r#"…"#` as a unit rather than a bare `"…"` string.
    raw_fixture = (
        'const DOC: &str = r#"# a toml comment with "quotes" and a // slash\n'
        'key = "value"\n'
        '"#;\n'
        "const AFTER: Kind = Kind::Bool;"
    )
    stripped_raw = strip_rust_comments(raw_fixture)
    if 'a toml comment with "quotes" and a // slash' not in stripped_raw:
        failures.append(
            f"strip_rust_comments: a raw string's own content was altered: {stripped_raw!r}"
        )
    if "const AFTER: Kind = Kind::Bool;" not in stripped_raw:
        failures.append(
            "strip_rust_comments: text after a raw string was lost or corrupted: "
            f"{stripped_raw!r}"
        )

    # value_until_char: a nested `;` inside braces/strings must not end the
    # scan early; a nix `''…''` string's OWN `;`-shaped escape must not either.
    val, end = value_until_char("foo { a; b; } ; bar", 0, ";")
    if val != "foo { a; b; } ":
        failures.append(f"value_until_char: nested `;` ended the scan early, got {val!r}")
    val, end = value_until_char("''line one; ''$ literal; still inside'' ; bar", 0, ";")
    if not val.startswith("''") or "still inside" not in val:
        failures.append(f"value_until_char: a nix `''…''` string's escapes were not honoured: {val!r}")

    # parse_kind: one fixture per variant, plus the two identifier-resolution
    # paths (`Kind::List(&IDENT)` and `Kind::Map(IDENT)`).
    fixture_src = '''
    pub const FIELDS: &[Field] = &[
        Field { path: "a", kind: Kind::Bool, doc: "a bool" },
    ];
    const APP_FIELDS: &[Field] = &[
        Field { path: "id", kind: Kind::Text { blank_ok: false }, doc: "an id" },
    ];
    const APP: Kind = Kind::Map(APP_FIELDS);
    pub const MIN_X: u64 = 1;
    pub const MAX_X: u64 = 9;
    '''
    ctx = RustCtx(fixture_src, "<fixture>")
    if parse_kind("Kind::Bool", ctx) != Kind("Bool"):
        failures.append("parse_kind: Kind::Bool")
    got_int = parse_kind('Kind::Int { min: 0, max: 64, also: &["rect"] }', ctx)
    if (got_int.min, got_int.max, got_int.also) != (0, 64, ("rect",)):
        failures.append(f"parse_kind: Kind::Int literal bounds/also wrong: {got_int}")
    got_int2 = parse_kind("Kind::Int { min: MIN_X.cast_signed(), max: MAX_X, also: &[] }", ctx)
    if (got_int2.min, got_int2.max, got_int2.also) != (1, 9, ()):
        failures.append(f"parse_kind: Kind::Int const-reference bounds wrong: {got_int2}")
    got_choice = parse_kind('Kind::Choice { options: &["a", "b"] }', ctx)
    if got_choice.options != ("a", "b"):
        failures.append(f"parse_kind: Kind::Choice options wrong: {got_choice}")
    got_text = parse_kind("Kind::Text { blank_ok: true }", ctx)
    if got_text.blank_ok is not True:
        failures.append(f"parse_kind: Kind::Text blank_ok wrong: {got_text}")
    got_color = parse_kind('Kind::Color { options: &["heat"] }', ctx)
    if got_color.options != ("heat",):
        failures.append(f"parse_kind: Kind::Color options wrong: {got_color}")
    got_list_inline = parse_kind("Kind::List(&Kind::Text { blank_ok: false })", ctx)
    if got_list_inline.variant != "List" or got_list_inline.elem.variant != "Text":
        failures.append(f"parse_kind: Kind::List(inline) wrong: {got_list_inline}")
    got_list_alias = parse_kind("Kind::List(&APP)", ctx)
    if got_list_alias.variant != "List" or got_list_alias.elem.variant != "Map":
        failures.append(f"parse_kind: Kind::List(&alias) did not resolve to a Map: {got_list_alias}")
    elif [f.path for f in got_list_alias.elem.fields] != ["id"]:
        failures.append(f"parse_kind: Kind::List(&alias)'s Map fields wrong: {got_list_alias.elem.fields}")
    got_map_ident = parse_kind("Kind::Map(APP_FIELDS)", ctx)
    if [f.path for f in got_map_ident.fields] != ["id"]:
        failures.append(f"parse_kind: Kind::Map(IDENT) wrong: {got_map_ident.fields}")

    # parse_schema_fields: the `fields: FIELDS` (identifier) shape.
    fields = parse_schema_fields(
        'pub const SCHEMA: Schema = Schema { family: "fixture", fields: FIELDS };\n' + fixture_src,
        "<fixture>",
    )
    if [f.path for f in fields] != ["a"]:
        failures.append(f"parse_schema_fields: identifier `fields:` shape wrong: {fields}")

    # parse_schema_fields: the inline `fields: &[ … ]` shape, with a nested
    # `Kind::Map(IDENT)` field — the `agents` shape.
    inline_src = fixture_src.replace(
        'pub const SCHEMA: Schema = Schema { family: "fixture", fields: FIELDS };',
        "",
    ) + '''
    pub const SCHEMA: Schema = Schema {
        family: "fixture",
        fields: &[
            Field { path: "socket", kind: Kind::Text { blank_ok: false }, doc: "a socket" },
            Field { path: "display", kind: Kind::Map(APP_FIELDS), doc: "a map" },
        ],
    };
    '''
    fields = parse_schema_fields(inline_src, "<fixture>")
    if [f.path for f in fields] != ["socket", "display"]:
        failures.append(f"parse_schema_fields: inline `fields: &[ … ]` shape wrong: {fields}")
    elif count_leaves(fields) != 3:
        failures.append(f"count_leaves: expected 3 (2 top + 1 nested), got {count_leaves(fields)}")

    # A Field literal missing a required key must raise, not silently parse.
    try:
        parse_field_literal('Field { path: "x", kind: Kind::Bool }', ctx)
        failures.append("parse_field_literal: a Field literal missing `doc:` was not refused")
    except LookupError:
        pass

    # card_fields!-shaped macro: a two-field template over two table names.
    macro_src = '''
    macro_rules! card_fields {
        ($($table:literal),*) => {
            &[$(
                Field {
                    path: concat!($table, ".cpu"),
                    kind: Kind::Bool,
                    doc: "cpu",
                },
                Field {
                    path: concat!($table, ".mem"),
                    kind: Kind::Bool,
                    doc: "mem",
                },
            )*]
        };
    }
    pub const SCHEMA: Schema = Schema {
        family: "fixture",
        fields: card_fields!("a", "b"),
    };
    '''
    fields = parse_schema_fields(macro_src, "<fixture>")
    if [f.path for f in fields] != ["a.cpu", "a.mem", "b.cpu", "b.mem"]:
        failures.append(f"parse_schema_fields: card_fields! macro expansion wrong: {fields}")

    # A `SCHEMA` that parses to zero fields must be distinguishable — the
    # zero-fields guard in `run_real_scan` relies on an EMPTY list coming
    # back rather than an exception, for an (admittedly pathological) empty
    # array.
    empty_src = 'pub const SCHEMA: Schema = Schema { family: "fixture", fields: &[] };'
    if parse_schema_fields(empty_src, "<fixture>") != []:
        failures.append("parse_schema_fields: an empty `fields: &[]` did not parse to []")

    # The nix option-tree parser: a flat leaf, a nested `Kind::Map`-shaped
    # leaf, and — the bug this design specifically guards against — a nested
    # sub-option's OWN `description` must never be read as the outer leaf's.
    nix_fixture = """
    config.fixture = lib.mkOption {
      type = lib.types.submodule {
        options = {
          flag = lib.mkOption {
            type = lib.types.nullOr lib.types.bool;
            default = null;
            description = ''
              A flag. More words after the first sentence.
            '';
          };

          rows = lib.mkOption {
            type = lib.types.nullOr (lib.types.ints.between 0 64);
            default = null;
            description = ''
              Rows in the lamp matrix.
            '';
          };

          also_rows = lib.mkOption {
            type = lib.types.nullOr (
              lib.types.either (lib.types.ints.between 0 64) (lib.types.enum [ "rect" ])
            );
            default = null;
            description = ''
              Rows, or the word.
            '';
          };

          style = lib.mkOption {
            type = lib.types.nullOr (lib.types.enum [ "a" "b" ]);
            default = null;
            description = ''
              The style.
            '';
          };

          socket = lib.mkOption {
            type = lib.types.nullOr (lib.types.strMatching "^/.+");
            default = null;
            description = ''
              An absolute path.
            '';
          };

          display = lib.mkOption {
            type = lib.types.attrsOf (
              lib.types.submodule {
                options = {
                  label = lib.mkOption {
                    type = lib.types.nullOr lib.types.str;
                    default = null;
                    description = ''
                      A nested label, whose own sentence must not leak upward.
                    '';
                  };
                };
              }
            );
            default = { };
            description = ''
              Per-agent display overrides.
            '';
          };
        };
      };
      default = { };
    };
    """
    nix_options = parse_family_options(nix_fixture, "config.fixture = lib.mkOption {")
    if set(nix_options) != {"flag", "rows", "also_rows", "style", "socket", "display"}:
        failures.append(f"parse_family_options: top-level leaf set wrong: {sorted(nix_options)}")
    if nix_options["display"].sub_options is None or set(nix_options["display"].sub_options) != {"label"}:
        failures.append("parse_family_options: `display`'s nested sub_options wrong")
    elif first_sentence(nix_options["display"].description_raw) != "Per-agent display overrides.":
        failures.append(
            "parse_family_options: a NESTED sub-option's description leaked into the outer "
            f"leaf's own — got {first_sentence(nix_options['display'].description_raw)!r}"
        )
    elif first_sentence(nix_options["display"].sub_options["label"].description_raw) != (
        "A nested label, whose own sentence must not leak upward."
    ):
        failures.append("parse_family_options: the nested `label` option's own description was not read")

    # check_kind_vs_nix_type: each variant, positive and negative.
    if check_kind_vs_nix_type(Kind("Bool"), nix_options["flag"], "<rust>"):
        failures.append("check_kind_vs_nix_type: Kind::Bool wrongly reported a mismatch")
    if not check_kind_vs_nix_type(Kind("Text", blank_ok=False), nix_options["flag"], "<rust>"):
        failures.append("check_kind_vs_nix_type: Kind::Text against a bool type was not caught")
    if check_kind_vs_nix_type(Kind("Int", min=0, max=64), nix_options["rows"], "<rust>"):
        failures.append("check_kind_vs_nix_type: Kind::Int (no also) wrongly reported a mismatch")
    if check_kind_vs_nix_type(Kind("Int", min=1, max=64), nix_options["rows"], "<rust>") == []:
        failures.append("check_kind_vs_nix_type: a moved Int bound was not caught")
    if check_kind_vs_nix_type(
        Kind("Int", min=0, max=64, also=("rect",)), nix_options["also_rows"], "<rust>"
    ):
        failures.append("check_kind_vs_nix_type: Kind::Int with also= wrongly reported a mismatch")
    if not check_kind_vs_nix_type(Kind("Int", min=0, max=64), nix_options["also_rows"], "<rust>"):
        failures.append("check_kind_vs_nix_type: a spurious `either`/`enum` (no `also`) was not caught")
    if check_kind_vs_nix_type(Kind("Choice", options=("a", "b")), nix_options["style"], "<rust>"):
        failures.append("check_kind_vs_nix_type: Kind::Choice wrongly reported a mismatch")
    if check_kind_vs_nix_type(Kind("Choice", options=("a", "c")), nix_options["style"], "<rust>") == []:
        failures.append("check_kind_vs_nix_type: a changed Choice option set was not caught")
    if check_kind_vs_nix_type(Kind("Text", blank_ok=False), nix_options["socket"], "<rust>"):
        failures.append("check_kind_vs_nix_type: Kind::Text against `strMatching` wrongly reported a mismatch")
    if check_kind_vs_nix_type(Kind("Map", fields=()), nix_options["display"], "<rust>"):
        failures.append("check_kind_vs_nix_type: Kind::Map wrongly reported a mismatch")
    if check_kind_vs_nix_type(Kind("Map", fields=()), nix_options["flag"], "<rust>") == []:
        failures.append("check_kind_vs_nix_type: Kind::Map against a scalar option was not caught")

    # first_sentence / strip_markdown: markdown stripped, sentence boundary
    # respects a colon/semicolon (not a sentence end) but stops at `. `.
    if first_sentence("The **hive**'s socket — an absolute path. More words.") != (
        "The hive's socket — an absolute path."
    ):
        failures.append("first_sentence: markdown emphasis or sentence boundary handled wrong")
    if first_sentence("One sentence only, no trailing period") != "One sentence only, no trailing period":
        failures.append("first_sentence: a description with no period lost content")

    # bracket_list_after / mount_wire_names / _enum_all_names — retained
    # unchanged from before #1375, still exercised here.
    mount_src = '''
    pub const ALL: [Mount; 3] = [Mount::SidebarLead, Mount::BarLeft, Mount::BarRight];
    #[must_use]
    pub fn wire_name(self) -> &'static str {
        match self {
            Mount::SidebarLead => "SidebarLead",
            Mount::BarLeft => "BarLeft",
            Mount::BarRight => "BarRight",
        }
    }
    '''
    got = mount_wire_names(mount_src)
    if got != ["SidebarLead", "BarLeft", "BarRight"]:
        failures.append(f"mount_wire_names: expected the three in ALL's order, got {got}")
    half_done = mount_src.replace('Mount::BarRight => "BarRight",', "")
    try:
        mount_wire_names(half_done)
        failures.append("mount_wire_names: a variant with no wire_name arm was not refused")
    except LookupError:
        pass
    mount_nix_src = '''
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
    '''
    got = bracket_list_after(mount_nix_src, "mount = lib.mkOption {")
    if got != ["SidebarLead", "BarLeft", "BarRight"]:
        failures.append(f"bracket_list_after: expected the three mount names, got {got}")

    # struct_serde_fields / places_option_levels — retained unchanged.
    struct_src = '''
    #[derive(serde::Deserialize)]
    struct PlaceCfg {
        name: String,
        lat: f64,
        #[serde(default)]
        ssids: Vec<String>,
    }

    #[derive(serde::Deserialize)]
    struct DeparturesCfg {
        #[serde(default)]
        endpoint: Option<String>,
    }
    '''
    got = struct_serde_fields(struct_src, "PlaceCfg")
    if got != ["name", "lat", "ssids"]:
        failures.append(f"struct_serde_fields: expected PlaceCfg's private fields, got {got}")
    not_allow_listed = struct_src.replace("struct PlaceCfg", "struct NotAllowListed")
    try:
        struct_serde_fields(not_allow_listed, "NotAllowListed")
        failures.append("struct_serde_fields: a private struct outside PRIVATE_STRUCTS was not refused")
    except LookupError:
        pass
    places_nix_src = '''
    config.places = lib.mkOption {
      type = lib.types.submodule {
        options = {
          place = lib.mkOption {
            type = lib.types.nullOr (
              lib.types.listOf (
                lib.types.submodule {
                  options = {
                    name = lib.mkOption { type = lib.types.str; };
                    lat = lib.mkOption { type = lib.types.float; };
                    ssids = lib.mkOption { type = lib.types.nullOr (lib.types.listOf lib.types.str); };
                  };
                }
              )
            );
            default = null;
          };
          departures = lib.mkOption {
            type = lib.types.submodule {
              options = {
                endpoint = lib.mkOption { type = lib.types.nullOr lib.types.str; };
              };
            };
            default = { };
          };
        };
      };
      default = { };
    };
    '''
    place_levels = places_option_levels(places_nix_src)
    if place_levels["PlaceCfg"] != ["name", "lat", "ssids"]:
        failures.append(f"places_option_levels: place leaves wrong, got {place_levels['PlaceCfg']}")
    if place_levels["DeparturesCfg"] != ["endpoint"]:
        failures.append(f"places_option_levels: departures leaves wrong, got {place_levels['DeparturesCfg']}")

    return failures


def _self_test_failed(lines: list[str]) -> int:
    """Print the "scanner disagrees with its own fixtures" verdict and
    return exit 2, the code `lint-bind-pins.py`/`lint-lints-tables.py`
    reserve for it — unchanged posture from before #1375 (see #1270)."""
    print("config-vocab scan: SELF-TEST FAILED", file=sys.stderr)
    for line in lines:
        print(f"  {line}", file=sys.stderr)
    print(
        "\nThe scanner disagrees with its own fixtures, so any verdict it gives on the\n"
        "tree is meaningless. Fix the extraction functions rather than the fixtures.",
        file=sys.stderr,
    )
    return 2


# ── `mutation_self_test()`: the whole real comparison, on real (mutated) nix


def read(path: str) -> str:
    with open(path, encoding="utf-8") as fh:
        return fh.read()


def read_rust_schema(path: str) -> str:
    """`read(path)` with `strip_rust_comments` applied — the entrypoint for
    the four `Field`/`Kind`-literal files (`CORE_LEDS_FAMILY_RS`,
    `WORKSPACES_FAMILY_RS`, `AGENTS_RS`, `STATS_RS`). `MANIFEST_RS`/
    `PLACES_RS` do NOT go through this: `_enum_all_names`/`struct_serde_fields`
    match whole-line syntax (`Ty::Variant => "…"`, `^\\s*(\\w+):`) a `//`
    comment cannot spuriously satisfy, so they keep reading with plain
    `read()`, exactly as before #1375."""
    return strip_rust_comments(read(path))


def mutate_drop_enum_value(nix_src: str, option_anchor: str, value: str) -> str:
    """One value removed from the FIRST `[ … ]` inside `option_anchor`'s own
    block — proves the leaf SET/enum comparison actually reds on a dropped
    enum member (ask #5, case 1)."""
    open_i, close_i = find_block_span(nix_src, option_anchor)
    block = nix_src[open_i:close_i]
    bracket_open = block.index("[")
    bracket_close = match_delim(block, bracket_open, "[", "]")
    body = block[bracket_open + 1 : bracket_close - 1]
    new_body, n = re.subn(rf'\s*"{re.escape(value)}"\n?', "", body, count=1)
    if n == 0:
        raise LookupError(f"value {value!r} not found in the enum list after anchor {option_anchor!r}")
    new_block = block[: bracket_open + 1] + new_body + block[bracket_close - 1 :]
    return nix_src[:open_i] + new_block + nix_src[close_i:]


def mutate_move_bound(nix_src: str, option_anchor: str) -> tuple[str, str, str]:
    """The FIRST `ints.between lo hi`'s upper bound incremented by one —
    proves the `Kind::Int` bound comparison reds on a moved bound (ask #5,
    case 2). Returns `(mutated_nix, old_hi, new_hi)` — both read out of THIS
    copy of the real file rather than assumed by the caller as a literal —
    so a legitimate future change to the bound this anchor happens to point
    at doesn't make the caller's own expectation stale (#1378 review,
    MEDIUM 2)."""
    open_i, close_i = find_block_span(nix_src, option_anchor)
    block = nix_src[open_i:close_i]
    m = re.search(r"ints\.between\s+(-?\d+)\s+(-?\d+)", block)
    if not m:
        raise LookupError(f"no `ints.between` found after anchor {option_anchor!r}")
    old_hi = m.group(2)
    new_hi = str(int(old_hi) + 1)
    new_block = block[: m.start(2)] + new_hi + block[m.end(2) :]
    return nix_src[:open_i] + new_block + nix_src[close_i:], old_hi, new_hi


def mutate_wrap_in_listof(nix_src: str, option_anchor: str) -> str:
    """`option_anchor`'s own `type = …;` attribute rewritten from
    `lib.types.nullOr X` to `lib.types.nullOr (lib.types.listOf X)` — proves
    a scalar `Kind` (`Text` here) reds against a nix type that WRAPS its
    expected shape in a combinator, rather than being satisfied by it the
    way a substring search would be (#1378 review, MEDIUM 4: the reviewer
    found `agents.display.label`'s real `nullOr str` silently accepted as
    `nullOr (listOf str)`, because the retired check searched for the
    substring `lib.types.str` ANYWHERE in the type expression, and that
    substring sits right there inside `listOf`'s own argument). `X` is
    read back out of THIS copy via `strip_null_or` rather than assumed, so
    this works on whatever scalar type the anchor's field currently has."""
    open_i, close_i = find_block_span(nix_src, option_anchor)
    block = nix_src[open_i:close_i]
    type_res = find_attr(block, "type", 0)
    if type_res is None:
        raise LookupError(f"no `type = …;` attribute found after anchor {option_anchor!r}")
    type_text, _start, _end = type_res
    core = strip_null_or(type_text.strip())
    if core == type_text.strip():
        raise LookupError(
            f"the type after anchor {option_anchor!r} is not `nullOr`-wrapped, as this "
            "mutation assumes"
        )
    new_type_text = f" lib.types.nullOr (lib.types.listOf {core})"
    new_block = block.replace(type_text, new_type_text, 1)
    return nix_src[:open_i] + new_block + nix_src[close_i:]


def mutate_add_unknown_family_block(nix_src: str, family: str) -> str:
    """`nix_src` with a fake, syntactically-plausible
    `config.<family> = lib.mkOption { … };` block appended at the very
    end — proves `unexpected_nix_surface` actually flags a family that
    gains a real nix block `FAMILY_NIX_ANCHORS` was never told about
    (#1378 review, MEDIUM 1). A plain append, not an insert at a specific
    nesting depth: `unexpected_nix_surface` only ever does a substring
    search for the anchor text, so where in the file it sits does not
    matter — only that the exact anchor shape `FAMILY_NIX_ANCHORS`' own
    values use is present somewhere."""
    fake = (
        f"\n      config.{family} = lib.mkOption {{\n"
        "        type = lib.types.attrsOf lib.types.str;\n"
        "        default = { };\n"
        '        description = "fake, for the self-test only";\n'
        "      };\n"
    )
    return nix_src + fake


def mutate_first_sentence(nix_src: str, option_anchor: str, needle: str, replacement: str) -> str:
    """One literal substring replaced inside `option_anchor`'s own block —
    proves the doc gate reds when a description's first sentence changes
    (ask #5, case 3)."""
    open_i, close_i = find_block_span(nix_src, option_anchor)
    block = nix_src[open_i:close_i]
    if needle not in block:
        raise LookupError(f"text {needle!r} not found in the block after anchor {option_anchor!r}")
    new_block = block.replace(needle, replacement, 1)
    return nix_src[:open_i] + new_block + nix_src[close_i:]


def mutate_remove_leaf(nix_src: str, option_anchor: str, leaf_name: str) -> str:
    """One `<leaf_name> = lib.mkOption { … };` removed whole from inside
    `option_anchor`'s own block — proves a `Kind::Map` sub-option comparison
    reds when a sub-option disappears (ask #5, case 4)."""
    open_i, close_i = find_block_span(nix_src, option_anchor)
    block = nix_src[open_i:close_i]
    leaf_anchor = f"{leaf_name} = lib.mkOption {{"
    leaf_start = block.find(leaf_anchor)
    if leaf_start < 0:
        raise LookupError(f"leaf {leaf_name!r} not found inside the block after anchor {option_anchor!r}")
    leaf_open = leaf_start + len(leaf_anchor) - 1
    leaf_close = match_delim(block, leaf_open, "{", "}")
    if leaf_close < 0:
        raise LookupError(f"leaf {leaf_name!r}'s own block never closes")
    m = re.match(r"\s*;\s*\n?", block[leaf_close:])
    end = leaf_close + (m.end() if m else 0)
    new_block = block[:leaf_start] + block[end:]
    return nix_src[:open_i] + new_block + nix_src[close_i:]


def mutation_self_test() -> list[str]:
    """The six cases ask #5 (plus #1378 review MEDIUM 1/MEDIUM 4) require,
    each against a real (mutated) copy of `nix/module-common.nix` and the
    REAL, unmutated Rust schema — see the module docs' "FALSIFYING THE
    COMPARISON" section.

    Every mutated VALUE (which enum member to drop, which bound to move to,
    which sentence to reword, which sub-field to remove) is read back out of
    the schema or the mutation's own return value — never a literal like
    `"oled"`/`"3600"`/`"project"` baked in here — so a legitimate future
    change to the schema (a new bound, a renamed sub-field, a reworded doc)
    cannot itself make a case's own EXPECTATION stale and turn a healthy
    self-test into a false "scan is broken" (#1378 review, MEDIUM 2:
    `MAX_POLL_SECONDS` moving, or `display.project` being renamed, used to
    fail this self-test with exit 2 and "fix the extraction functions" —
    advice that was wrong, since the scan itself was fine). WHICH field a
    case mutates is still a fixed choice (case 1 always targets
    `core-leds.style`, case 4 always targets `agents.display`'s LAST
    sub-field, …) — that is test design, not a value that drifts."""
    failures: list[str] = []
    real_nix = read(MODULE_COMMON_NIX)
    core_leds_src = read_rust_schema(CORE_LEDS_FAMILY_RS)
    agents_src = read_rust_schema(AGENTS_RS)
    core_leds_fields = parse_schema_fields(core_leds_src, CORE_LEDS_FAMILY_RS)
    agents_fields = parse_schema_fields(agents_src, AGENTS_RS)

    def field(fields: list[Field], path: str) -> Field:
        return next(f for f in fields if f.path == path)

    def run(name: str, family: str, rust_file: str, fields: list[Field], mutated_nix: str, expect: list[str]) -> None:
        mismatches: list[str] = []
        try:
            nix_options = parse_family_options(mutated_nix, FAMILY_NIX_ANCHORS[family])
            compare(family, rust_file, fields, nix_options, mismatches)
        except LookupError as e:
            failures.append(f"self-test case {name!r}: the scan raised {e!r} instead of a mismatch")
            return
        joined = "\n".join(mismatches)
        missing = [s for s in expect if s not in joined]
        if not mismatches:
            failures.append(f"self-test case {name!r}: expected a red mismatch, got none")
        elif missing:
            failures.append(
                f"self-test case {name!r}: reds, but is missing expected substring(s) {missing} in:\n{joined}"
            )
        else:
            print(f"  ok    self-test: {name} — {len(mismatches)} mismatch(es), as expected")

    style_field = field(core_leds_fields, "style")
    dropped = style_field.kind.options[-1]
    m1 = mutate_drop_enum_value(real_nix, "style = lib.mkOption {", dropped)
    run(
        f"enum value dropped (core-leds.style loses {dropped!r})",
        "core-leds",
        CORE_LEDS_FAMILY_RS,
        core_leds_fields,
        m1,
        ["core-leds.style", "nix/module-common.nix", CORE_LEDS_FAMILY_RS, dropped],
    )

    m2, old_hi, new_hi = mutate_move_bound(real_nix, "poll_seconds = lib.mkOption {")
    run(
        f"bound moved (agents.poll_seconds's upper bound {old_hi} -> {new_hi})",
        "agents",
        AGENTS_RS,
        agents_fields,
        m2,
        ["agents.poll_seconds", "nix/module-common.nix", AGENTS_RS, old_hi, new_hi],
    )

    fill_field = field(core_leds_fields, "fill")
    needle = first_sentence(fill_field.doc)
    replacement = (needle[:-1] if needle.endswith(".") else needle) + " (mutated for the self-test)."
    m3 = mutate_first_sentence(real_nix, "fill = lib.mkOption {", needle, replacement)
    run(
        "description first sentence changed (core-leds.fill)",
        "core-leds",
        CORE_LEDS_FAMILY_RS,
        core_leds_fields,
        m3,
        ["core-leds.fill", "nix/module-common.nix", CORE_LEDS_FAMILY_RS],
    )

    display_field = field(agents_fields, "display")
    removed_leaf = display_field.kind.fields[-1].path
    m4 = mutate_remove_leaf(real_nix, "display = lib.mkOption {", removed_leaf)
    run(
        f"Map sub-option removed (agents.display loses `{removed_leaf}`)",
        "agents",
        AGENTS_RS,
        agents_fields,
        m4,
        [f"agents.display.{removed_leaf}", "nix/module-common.nix", AGENTS_RS],
    )

    m5 = mutate_add_unknown_family_block(real_nix, "workspaces")
    known = {"core-leds", "agents"}
    surprising = unexpected_nix_surface(m5, ["core-leds", "agents", "workspaces", "stats"], FAMILY_NIX_ANCHORS)
    name5 = "unknown nix surface (workspaces gains a real config.workspaces block)"
    if surprising != ["workspaces"]:
        failures.append(
            f"self-test case {name5!r}: expected `unexpected_nix_surface` to flag exactly "
            f"['workspaces'], got {surprising}"
        )
    elif set(FAMILY_NIX_ANCHORS) != known:
        failures.append(
            f"self-test case {name5!r}: FAMILY_NIX_ANCHORS's keys are {sorted(FAMILY_NIX_ANCHORS)}, "
            f"not the {sorted(known)} this case assumes — update the fixture alongside the anchors table"
        )
    else:
        print(f"  ok    self-test: {name5} — flagged, as expected")

    m6 = mutate_wrap_in_listof(real_nix, "label = lib.mkOption {")
    run(
        "scalar type collision (agents.display.label's `str` wrapped in `listOf`)",
        "agents",
        AGENTS_RS,
        agents_fields,
        m6,
        ["agents.display.label", "nix/module-common.nix", AGENTS_RS, "Kind::Text"],
    )

    return failures


MUTATION_CASE_COUNT = 6


def run_self_tests() -> int:
    """Both self-test layers, `--self-test`'s own entrypoint: exits 2 on
    EITHER layer's failure and runs no real scan — the `--self-test` flag
    asked for just the self-test layers in isolation. `main()`'s default
    (non-`--self-test`) path does NOT call this; see its own docstring for
    why a mutation-layer failure there behaves differently (#1378 review,
    MEDIUM 2)."""
    try:
        failures = self_test()
    except Exception as e:  # noqa: BLE001 - narrower than this would mask a bug in the fixtures themselves
        return _self_test_failed([f"a `self_test` fixture raised {type(e).__name__}: {e}"])
    if failures:
        return _self_test_failed(failures)
    print("config-vocab self-test: unit fixtures ok", flush=True)

    try:
        mut_failures = mutation_self_test()
    except Exception as e:  # noqa: BLE001
        return _self_test_failed([f"`mutation_self_test` raised {type(e).__name__}: {e}"])
    if mut_failures:
        return _self_test_failed(mut_failures)
    print(
        f"config-vocab self-test: all {MUTATION_CASE_COUNT} mutation cases reported red, as expected",
        flush=True,
    )
    return 0


def run_real_scan() -> int:
    missing = [
        p
        for p in (
            CORE_LEDS_FAMILY_RS,
            WORKSPACES_FAMILY_RS,
            AGENTS_RS,
            STATS_RS,
            MANIFEST_RS,
            PLACES_RS,
            MODULE_COMMON_NIX,
        )
        if not os.path.isfile(p)
    ]
    if missing:
        print(f"config-vocab scan: file(s) not found: {', '.join(missing)}", file=sys.stderr)
        print("  (run from inside the repository)", file=sys.stderr)
        return 2

    core_leds_src = read_rust_schema(CORE_LEDS_FAMILY_RS)
    workspaces_src = read_rust_schema(WORKSPACES_FAMILY_RS)
    agents_src = read_rust_schema(AGENTS_RS)
    stats_src = read_rust_schema(STATS_RS)
    manifest_src = read(MANIFEST_RS)
    places_src = read(PLACES_RS)
    nix_src = read(MODULE_COMMON_NIX)

    try:
        core_leds_fields = parse_schema_fields(core_leds_src, CORE_LEDS_FAMILY_RS)
        workspaces_fields = parse_schema_fields(workspaces_src, WORKSPACES_FAMILY_RS)
        agents_fields = parse_schema_fields(agents_src, AGENTS_RS)
        stats_fields = parse_schema_fields(stats_src, STATS_RS)
    except LookupError as e:
        print(f"config-vocab scan: {e}", file=sys.stderr)
        print(
            "  (a `Field`/`Kind` literal this script depends on has moved or been reworded — "
            "update nix/lint-config-vocab.py to match)",
            file=sys.stderr,
        )
        return 2

    counts = {
        "core-leds": count_leaves(core_leds_fields),
        "workspaces": count_leaves(workspaces_fields),
        "agents": count_leaves(agents_fields),
        "stats": count_leaves(stats_fields),
    }
    zero = [name for name, n in counts.items() if n == 0]
    if zero:
        print(
            f"config-vocab scan: {', '.join(zero)}'s SCHEMA parsed to zero fields — the "
            "parser is broken, or the schema really is empty and this guard needs revisiting",
            file=sys.stderr,
        )
        return 2

    surprising = unexpected_nix_surface(nix_src, list(counts), FAMILY_NIX_ANCHORS)
    if surprising:
        print(
            f"config-vocab scan: {', '.join(surprising)} now ha"
            f"{'s' if len(surprising) == 1 else 've'} a `config.<family> = lib.mkOption "
            "{ … };` block in nix/module-common.nix, but FAMILY_NIX_ANCHORS does not know "
            "about it — this scan would otherwise report it \"skipped: no nix surface\" and "
            "exit 0 while that block's vocabulary drifts entirely unchecked",
            file=sys.stderr,
        )
        print(
            "  (add an anchor for it to FAMILY_NIX_ANCHORS in nix/lint-config-vocab.py, and "
            "wire the family into the compare() calls below)",
            file=sys.stderr,
        )
        return 2

    mismatches: list[str] = []
    try:
        core_leds_nix = parse_family_options(nix_src, FAMILY_NIX_ANCHORS["core-leds"])
        compare("core-leds", CORE_LEDS_FAMILY_RS, core_leds_fields, core_leds_nix, mismatches)

        agents_nix = parse_family_options(nix_src, FAMILY_NIX_ANCHORS["agents"])
        compare("agents", AGENTS_RS, agents_fields, agents_nix, mismatches)

        rust_mount = mount_wire_names(manifest_src)
        nix_mount = bracket_list_after(nix_src, "mount = lib.mkOption {")
        if nix_mount != rust_mount:
            mismatches.append(
                f"mount: nix/module-common.nix has {nix_mount}, but Mount::ALL is {rust_mount} "
                f"({MANIFEST_RS}) — a value only one side knows renders a HYTTE_PLUGIN_MOUNT the "
                "SDK refuses at plugin startup"
            )

        rust_places_levels = compare_places(nix_src, places_src, mismatches)
    except LookupError as e:
        print(f"config-vocab scan: {e}", file=sys.stderr)
        print(
            "  (an anchor this script depends on has moved or been reworded — update "
            "nix/lint-config-vocab.py to match)",
            file=sys.stderr,
        )
        return 2

    if mismatches:
        print(
            f"\nERROR: {len(mismatches)} config vocabulary mismatch(es) between "
            "nix/module-common.nix and trollshell's Rust schemas:\n",
            file=sys.stderr,
        )
        for line in mismatches:
            print(f"  - {line}", file=sys.stderr)
        print(
            "\nnix/module-common.nix's `programs.trollshell.config.{core-leds,agents,places}` "
            "and `plugins.<id>.mount`\nhand-mirror this vocabulary — update whichever side "
            "fell behind so a base-layer render and\nthe shell's own schema/parser agree.",
            file=sys.stderr,
        )
        return 1

    print(
        "config-vocab scan: "
        f"core-leds {counts['core-leds']} leaf field(s) — nix agrees; "
        f"agents {counts['agents']} leaf field(s) — nix agrees; "
        f"workspaces {counts['workspaces']} leaf field(s), skipped: no nix surface (#1374 "
        "would add one); "
        f"stats {counts['stats']} leaf field(s), skipped: no nix surface (#1374 would add one); "
        f"places {rust_places_levels['PlaceCfg']} + departures {rust_places_levels['DeparturesCfg']} "
        "— nix agrees (untouched #1339 check); "
        f"plugins.<id>.mount {rust_mount} — nix agrees",
        flush=True,
    )
    return 0


def main() -> int:
    """`--self-test` runs both self-test layers in isolation
    (`run_self_tests`) and stops there, exit 2 on either's failure — see
    that function's docstring.

    The default path is NOT "run_self_tests() then, if clean, run the real
    scan": a UNIT-fixture failure (`self_test()`, the parsing primitives
    against small hand-built fixtures) is self-contained — "the scan itself
    is untrustworthy", exit 2, no real scan, same as before #1378. A
    MUTATION-fixture failure (`mutation_self_test()`, the whole comparison
    against a mutated copy of the REAL file) is different: it can mean the
    scan is broken, but it can equally mean the schema legitimately moved
    out from under one case's assumption in a way this self-test's own
    dynamic derivation (see `mutation_self_test`'s docstring) didn't cover
    — and in EITHER case, the real scan below is still answerable and its
    answer is still useful, so it runs and prints its own verdict rather
    than leaving the operator staring at a self-test failure with no idea
    whether `nix/module-common.nix` itself is currently drifted (#1378
    review, MEDIUM 2). The overall exit code is still 2 either way — the
    self-test's own trustworthiness is what failed, not `nix/module-common.nix`."""
    if "--self-test" in sys.argv[1:]:
        return run_self_tests()

    try:
        unit_failures = self_test()
    except Exception as e:  # noqa: BLE001 - narrower than this would mask a bug in the fixtures themselves
        return _self_test_failed([f"a `self_test` fixture raised {type(e).__name__}: {e}"])
    if unit_failures:
        return _self_test_failed(unit_failures)
    print("config-vocab self-test: unit fixtures ok", flush=True)

    try:
        mut_failures = mutation_self_test()
    except Exception as e:  # noqa: BLE001
        mut_failures = [f"`mutation_self_test` raised {type(e).__name__}: {e}"]

    if mut_failures:
        print("config-vocab scan: SELF-TEST FAILED (mutation layer)", file=sys.stderr)
        for line in mut_failures:
            print(f"  {line}", file=sys.stderr)
        print(
            "\nThe mutation self-test disagreed with its own fixtures. Unlike a unit-fixture\n"
            "failure this is not necessarily self-contained — it can also mean the schema\n"
            "legitimately moved out from under a mutation case's own assumption — so the real\n"
            "scan below still ran; read its own verdict on its own merits.\n",
            file=sys.stderr,
        )
        run_real_scan()
        return 2

    print(
        f"config-vocab self-test: all {MUTATION_CASE_COUNT} mutation cases reported red, as expected",
        flush=True,
    )
    return run_real_scan()


if __name__ == "__main__":
    sys.exit(main())
