#!/usr/bin/env python3
"""Fail if the claude-bridge socket name drifts between nix and Rust.

THE DEFECT
----------
`hytte-claude-bridge` binds `$XDG_RUNTIME_DIR/trollshell/claude-bridge.sock`
(#993/#1099). That name is spelled out in five places, and only some of them
are compiler-checked against each other:

  - `crates/hytte-ai-providers/src/unix.rs`'s `BRIDGE_SOCKET_DIR`/
    `BRIDGE_SOCKET_FILE` constants and their composed `BRIDGE_BASE_URL`.
    **Already pinned against raw literals by plain `cargo test`** —
    `bridge_url_resolves_to_the_bridge_socket_path` (`unix.rs`) asserts
    `BRIDGE_SOCKET_DIR == "trollshell"` and `BRIDGE_SOCKET_FILE ==
    "claude-bridge.sock"` directly, and `the_socket_path_is_the_one_the_
    client_dials` (`crates/hytte-claude-bridge/src/socket.rs`) pins the same
    literal against the path the daemon actually binds. Neither test is
    feature-gated, so both run under a plain `cargo test` and under
    `nix build .#trollshell`'s `doCheck`.
  - `crates/hytte-ai-providers/src/unix.rs`'s own module doc (`//!` lines) —
    a worked example of `BRIDGE_BASE_URL`, in prose. No test reads a doc
    comment.
  - `crates/hytte-claude-bridge/src/main.rs`'s module doc — a sentence
    stating the same path in prose, in a *different* crate, so not even the
    same compilation unit ties it to the constants above.
  - `nix/module-common.nix`'s `programs.trollshell.claudeBridge.baseUrl`
    `default` — a plain nix string literal. Nix cannot `import` a Rust
    constant, so nothing spans this seam at all.

So this script is deliberately **not** closing "the Rust side might drift
internally" — that ground is already covered. It closes the two seams
nothing else reaches: nix-to-Rust (what #1099's review actually measured —
see below), and the doc prose in both crates, which is real text a reader
acts on but no compiler or test ever opens.

#1099's review (M1) measured the nix-to-Rust gap directly: renaming
`BRIDGE_SOCKET_DIR`/`BRIDGE_SOCKET_FILE` (and `BRIDGE_BASE_URL`, kept in step
by hand) left `cargo test`, `cargo clippy` and every module-eval check green,
because the nix literal and the Rust constants sit on opposite sides of a
seam no compiler spans. A live session would notice — the daemon binds a
path no plugin dials — but CI would not.

This is a `runCommand` source scan on the `nix/lint-glsl.py` /
`nix/lint-bind-pins.py` / `nix/lint-core-leds-vocab.py` precedent, wired as
`checks.bridge-socket` in `flake.nix`.

WHAT IT CHECKS
--------------
Every one of the five sites above is reduced to the same comparison: does it
name `BRIDGE_SOCKET_DIR/BRIDGE_SOCKET_FILE` composed (the "expected suffix",
e.g. `trollshell/claude-bridge.sock`)? Two of the five need care reading, for
two different reasons that earlier revisions of this script got wrong (see
`crates/hytte-claude-bridge/src/main.rs`'s and `nix/module-common.nix`'s
extraction functions below for the mechanics):

  - `main.rs`'s doc sentence is not the *first* `$XDG_RUNTIME_DIR`-rooted
    path in the file — `socket.rs` documents the neighbouring
    `…claude-bridge.sock.lock` file, and a plausible-looking correct
    cross-reference could just as easily be added a paragraph earlier. A
    scan that takes "the first match in the file" can be fooled by line
    order in *either* direction — it silently stops checking once it finds
    one, whether or not that one is the sentence actually describing the
    bind path. This script anchors on the specific sentence (the stable
    phrase `"bound to"`, pinned as `MAIN_RS_DOC_ANCHOR` below and required to
    appear verbatim in `main.rs`'s module doc) and reads only the
    `$XDG_RUNTIME_DIR`-rooted path(s) in *that* doc-comment paragraph,
    starting *after* the anchor phrase — never a paragraph before or after
    it. A doc edit that removes the sentence (or the anchor phrase) entirely
    is a scan failure (exit 2), not a silent pass.
  - `nix/module-common.nix`'s `claudeBridge.baseUrl` `default` is read from
    inside that **specific option's own `{ … }` block**, brace-matched from
    `baseUrl = lib.mkOption {` to its closing `}` (the `lint-bind-pins.py`/
    `lint-core-leds-vocab.py` `match_delim` shape) — not "the next `default =
    "…"` found anywhere after the word `baseUrl`". `module-common.nix`
    declares several string-valued options; an unbounded forward search
    would read a sibling option's default (e.g. `mode`'s, four options
    later) if `baseUrl`'s own `default` line were ever deleted, and name the
    wrong option in the error.

Beyond those two anchored reads, this script also scans two nearby text
blocks wholesale for every `$XDG_RUNTIME_DIR`-rooted path they mention,
because both are files this script already opens and both carry prose
copies of the path that could drift independently of the machine-read
values above:

  - `crates/hytte-ai-providers/src/unix.rs`'s `//!` module-doc lines (its own
    worked example of the URL, separate from `BRIDGE_BASE_URL` itself).
    Restricted to `//!`-prefixed lines specifically, **not** the whole file:
    this crate's own test module deliberately dials a *different*,
    intentionally-wrong path (`"…/trollshell/x.sock"`, in
    `chat_refuses_an_unresolvable_socket_url_instead_of_falling_back`) to
    exercise its error path, and that literal must never be compared against
    the real socket name. `///` item-doc comments are excluded too, on the
    same reasoning applied more conservatively (nothing there currently
    needs the check, and excluding them costs nothing).
  - `nix/module-common.nix`, scanned whole-file: unlike `unix.rs`, nothing in
    this file intentionally names a *different* `$XDG_RUNTIME_DIR`-rooted
    path today (verified by inspection — every match in the file today is
    the bridge's own), so an unrestricted scan is safe and simpler than
    bounding three separate comment/string regions individually. If a future
    subsystem adds its own `$XDG_RUNTIME_DIR`-rooted default to this file,
    that assumption stops holding and this scan will need scoping the same
    way `unix.rs`'s does — it does not need it yet.

Both `$XDG_RUNTIME_DIR` and its braced spelling `${XDG_RUNTIME_DIR}` are
accepted everywhere (`hytte_ai_providers::RUNTIME_DIR_TOKENS`, `unix.rs:91`,
pinned equivalent by `the_runtime_dir_token_is_expanded_in_both_spellings`):
every extraction here captures only the path *after* the token, so the two
spellings compare equal without any extra normalisation step.

**Out of scope, named rather than silently skipped:** `nix/hm-module.nix`,
`crates/hytte-plugin-pet/src/brain.rs`, `crates/hytte-plugin-caw/src/
briefing.rs`, `etc/systemd/user/README.md`, `docs/live-verify.md`, and
`CLAUDE.md` all also mention this path in prose. None of them is a source
this lint reads for its own sake (deployment docs and consumer defaults, not
the wire contract), so a drift there is not caught here. `flake.nix`'s
`PET_LLM_URL` fixture is asserted equal to `claudeBridge.baseUrl` by the
`hm-module` flake check already, and `trollshell/src/plugin_launcher.rs`'s
mention is a test fixture, not a claim about the real value — neither needs
this script's help.

WHY THIS IS A NIX LINT AND NOT A `cargo test`
----------------------------------------------
A `cargo test` cannot read `nix/module-common.nix` inside the sandboxed
`workspace` derivation `nix build` runs its checks in: `nix/package.nix`'s
crane source filter keeps only `.rs`/`.toml`/`Cargo.lock`,
`assets/hytte-ui/style.css` and `.vert`/`.frag` files, so no `.nix` file
exists in that build's source tree at all (the same `include_str!`-of-
`assets/` trap CLAUDE.md documents, reached here by `std::fs::read_to_string`
instead). Widening the crane filter to keep `*.nix` was rejected for
`core-leds-vocab` for the same reason it would be rejected here: every edit
to *any* `.nix` file would invalidate the `workspace` derivation's source
hash and force a full recompile.

A plain `pkgs.runCommand` (the `bind-pins`/`glsl`/`core-leds-vocab`
precedent) reads the real repository tree — there is no crane filter between
a `runCommand`'s `src = ./.;`-shaped input and the checkout — needs no
compile, and reds in seconds.

Run it by hand from the repo root with:

    nix shell nixpkgs#python3 --command python3 nix/lint-bridge-socket.py

The `nix shell` is not optional: **`python3` is deliberately not on the
devShell PATH** (see `nix/lint-bind-pins.py`'s own header), so a bare
`python3 nix/lint-bridge-socket.py` is `command not found`.

USAGE
-----
    python3 nix/lint-bridge-socket.py

Exits 0 when every site names the same path, 1 naming every mismatch found,
2 when the scan itself is untrustworthy (a source file is missing, an anchor
cannot be found, or `self_test()` — run first, on every invocation —
disagrees with its own fixtures).
"""

import os
import re
import sys

REPO_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

UNIX_RS = os.path.join(REPO_ROOT, "crates", "hytte-ai-providers", "src", "unix.rs")
BRIDGE_MAIN_RS = os.path.join(REPO_ROOT, "crates", "hytte-claude-bridge", "src", "main.rs")
MODULE_COMMON_NIX = os.path.join(REPO_ROOT, "nix", "module-common.nix")

# The stable phrase `main.rs`'s module doc uses to introduce the bind path —
# see "WHAT IT CHECKS" above. Must appear verbatim in that file's doc, once,
# immediately (module-doc-paragraph-wise) before the path literal.
MAIN_RS_DOC_ANCHOR = "bound to"

# `$XDG_RUNTIME_DIR/…` or `${XDG_RUNTIME_DIR}/…` — both are the same
# environment variable (`RUNTIME_DIR_TOKENS`, `unix.rs:91`); only the path
# *after* the token is captured, so the two spellings are already the same
# value to every comparison in this script.
RUNTIME_DIR_PATH_RE = re.compile(r"\$\{?XDG_RUNTIME_DIR\}?/([A-Za-z0-9_.\-/]+)")


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


def str_const(src: str, name: str) -> str:
    """The string literal a `pub const NAME: &str = "...";` binds.

    Anchored on the identifier with a `\\b` before it and `\\s*:` right after,
    so a differently-named constant sharing a prefix or suffix with `name`
    can never match instead. `\\s` already spans a rustfmt-inserted newline
    between `=` and the opening quote.
    """
    m = re.search(rf'\bconst\s+{re.escape(name)}\s*:\s*&str\s*=\s*"([^"]*)"', src)
    if not m:
        raise LookupError(f"const {name}: &str = \"...\" not found")
    return m.group(1)


def paths_after_runtime_dir_token(text: str) -> list[str]:
    """Every path-like suffix following a `$XDG_RUNTIME_DIR`/`${XDG_RUNTIME_DIR}`
    token in `text`, in order."""
    return [m.group(1) for m in RUNTIME_DIR_PATH_RE.finditer(text)]


def doc_comment_paragraphs(src: str, marker: str = "//!") -> list[str]:
    """`marker`-prefixed lines, grouped into paragraphs (a blank `marker`
    line ends one), each flattened to one line (the prefix stripped, wrapped
    lines joined by a single space) so a sentence rustfmt wraps across lines
    reads as continuous text.

    `marker` defaults to `"//!"` (module docs). A `"///"` item-doc line does
    not start with `"//!"` as a literal string, so item docs are excluded by
    construction, not by a second check.
    """
    paragraphs = []
    current: list[str] = []
    for line in src.splitlines():
        stripped = line.strip()
        if not stripped.startswith(marker):
            continue
        content = stripped[len(marker) :]
        if content.startswith(" "):
            content = content[1:]
        if content == "":
            if current:
                paragraphs.append(" ".join(current))
                current = []
        else:
            current.append(content)
    if current:
        paragraphs.append(" ".join(current))
    return paragraphs


def anchored_paragraph_paths(src: str, anchor: str, marker: str = "//!") -> list[str]:
    """`paths_after_runtime_dir_token`, scoped to the text *following*
    `anchor` within whichever `marker`-comment paragraph contains it.

    Deliberately not "the first `$XDG_RUNTIME_DIR`-rooted path in the file":
    a line naming a related-but-different path (a lock file, a second
    example) placed before this paragraph — or before the anchor within the
    same paragraph — must never be picked up instead of the sentence the
    anchor names. Scoping to "after the anchor, within its own paragraph" is
    immune to both directions of that line-order dependence.
    """
    for para in doc_comment_paragraphs(src, marker):
        idx = para.find(anchor)
        if idx < 0:
            continue
        region = para[idx + len(anchor) :]
        found = paths_after_runtime_dir_token(region)
        if not found:
            raise LookupError(
                f"anchor {anchor!r} found, but no $XDG_RUNTIME_DIR-rooted path follows "
                "it in that doc paragraph"
            )
        return found
    raise LookupError(f"anchor phrase {anchor!r} not found in any {marker!r} paragraph")


def all_doc_paragraph_paths(src: str, marker: str = "//!") -> list[str]:
    """`paths_after_runtime_dir_token` over every `marker`-comment paragraph,
    unanchored — for a file whose `marker`-comment mentions of a
    `$XDG_RUNTIME_DIR`-rooted path are all supposed to name the same one.
    Restricting to `marker`-prefixed lines (rather than the whole file) is
    what excludes a crate's own test-fixture literals — see "WHAT IT CHECKS"
    in the module doc above.
    """
    found: list[str] = []
    for para in doc_comment_paragraphs(src, marker):
        found.extend(paths_after_runtime_dir_token(para))
    return found


def nix_option_block(src: str, anchor: str) -> str:
    """The text of one `<name> = lib.mkOption { ... };` block, brace-matched
    from `anchor` (which must end at the block's opening `{`) to its closing
    `}` — so a `default = "..."` sitting in a LATER option can never be
    picked up as this option's value (the `lint-bind-pins.py`/
    `lint-core-leds-vocab.py` `match_delim` shape).
    """
    start = src.find(anchor)
    if start < 0:
        raise LookupError(f"anchor {anchor!r} not found")
    open_i = start + len(anchor) - 1
    if src[open_i] != "{":
        raise LookupError(f"anchor {anchor!r} does not end at an opening '{{'")
    close_i = match_delim(src, open_i, "{", "}")
    if close_i < 0:
        raise LookupError(f"anchor {anchor!r}'s opening '{{' never closes")
    return src[start:close_i]


def nix_option_default_str(src: str, anchor: str) -> str:
    """The string literal a `default = "...";` binds, scoped to the single
    option block named by `anchor` (see `nix_option_block`) — never a
    sibling option's default.
    """
    block = nix_option_block(src, anchor)
    m = re.search(r'default\s*=\s*"([^"]*)"', block)
    if not m:
        raise LookupError(f'no default = "..." found inside the option block anchored at {anchor!r}')
    return m.group(1)


def compose_base_url(dir_: str, file_: str) -> str:
    return f"unix://$XDG_RUNTIME_DIR/{dir_}/{file_}"


# Fixtures for `self_test()`, run on every invocation. A clean tree proves
# nothing about whether the extraction functions still work — only a case
# built to disagree can tell the two apart (the same reasoning
# `lint-bind-pins.py`'s header gives for its own fixtures).
def self_test_str_const() -> list[str]:
    failures = []
    unix_src = """
    pub const BRIDGE_SOCKET_DIR_LEGACY: &str = "old-trollshell";
    pub const BRIDGE_SOCKET_DIR: &str = "trollshell";
    pub const BRIDGE_SOCKET_FILE: &str = "claude-bridge.sock";
    pub const BRIDGE_BASE_URL: &str = "unix://$XDG_RUNTIME_DIR/trollshell/claude-bridge.sock";
    """
    got_dir = str_const(unix_src, "BRIDGE_SOCKET_DIR")
    if got_dir != "trollshell":
        failures.append(f"str_const: expected 'trollshell', matched the legacy decoy: {got_dir!r}")
    got_file = str_const(unix_src, "BRIDGE_SOCKET_FILE")
    if got_file != "claude-bridge.sock":
        failures.append(f"str_const: expected 'claude-bridge.sock', got {got_file!r}")
    got_base_url = str_const(unix_src, "BRIDGE_BASE_URL")
    if got_base_url != "unix://$XDG_RUNTIME_DIR/trollshell/claude-bridge.sock":
        failures.append(f"str_const: BRIDGE_BASE_URL mismatch, got {got_base_url!r}")
    return failures


def self_test_runtime_dir_token() -> list[str]:
    failures = []
    bare = paths_after_runtime_dir_token("unix://$XDG_RUNTIME_DIR/trollshell/claude-bridge.sock")
    if bare != ["trollshell/claude-bridge.sock"]:
        failures.append(f"paths_after_runtime_dir_token: bare spelling gave {bare!r}")
    braced = paths_after_runtime_dir_token("unix://${XDG_RUNTIME_DIR}/trollshell/claude-bridge.sock")
    if braced != ["trollshell/claude-bridge.sock"]:
        failures.append(f"paths_after_runtime_dir_token: braced spelling gave {braced!r}")
    multi = paths_after_runtime_dir_token(
        "see $XDG_RUNTIME_DIR/trollshell/claude-bridge.sock and also "
        "${XDG_RUNTIME_DIR}/trollshell/claude-bridge.sock.lock"
    )
    if multi != ["trollshell/claude-bridge.sock", "trollshell/claude-bridge.sock.lock"]:
        failures.append(f"paths_after_runtime_dir_token: multi-match gave {multi!r}")
    none_ = paths_after_runtime_dir_token("no runtime dir token in this sentence at all")
    if none_:
        failures.append(f"paths_after_runtime_dir_token: expected no matches, got {none_!r}")
    return failures


def self_test_main_rs_anchor() -> list[str]:
    failures = []

    # A11 mirror: a correct-looking cross-reference in an EARLIER, separate
    # paragraph must never be picked up in place of the anchor paragraph's
    # own (here also correct) path.
    fixture_correct = """
    //! (Compare the plugin host socket beside it: `$XDG_RUNTIME_DIR/trollshell/claude-bridge.sock`.)
    //!
    //! One route, `POST /v1/chat/completions`, bound to
    //! `$XDG_RUNTIME_DIR/trollshell/claude-bridge.sock` — the path is not
    //! configurable.
    """
    got = anchored_paragraph_paths(fixture_correct, MAIN_RS_DOC_ANCHOR)
    if got != ["trollshell/claude-bridge.sock"]:
        failures.append(f"anchored_paragraph_paths: case A (earlier correct paragraph) gave {got!r}")

    # A11 exact: same shape, but the anchor paragraph's OWN path is drifted.
    # Must report the drifted value, not silently fall back to the earlier
    # paragraph's correct-looking one (the fail-open bug this replaces).
    fixture_drifted = """
    //! (Compare the plugin host socket beside it: `$XDG_RUNTIME_DIR/trollshell/claude-bridge.sock`.)
    //!
    //! One route, `POST /v1/chat/completions`, bound to
    //! `$XDG_RUNTIME_DIR/trollshell/claude-bridge-STALE.sock` — the path is not
    //! configurable.
    """
    got = anchored_paragraph_paths(fixture_drifted, MAIN_RS_DOC_ANCHOR)
    if got != ["trollshell/claude-bridge-STALE.sock"]:
        failures.append(
            f"anchored_paragraph_paths: case B (anchor paragraph drifted) gave {got!r}, "
            "expected the drifted value, not the earlier paragraph's correct one"
        )

    # A10 mirror: a related-but-different path (the lock file) named in an
    # earlier paragraph must not cause a false mismatch against the anchor
    # paragraph's own (correct) path.
    fixture_lock_before = """
    //! The single-instance lock sits beside the socket at
    //! `$XDG_RUNTIME_DIR/trollshell/claude-bridge.sock.lock`.
    //!
    //! One route, `POST /v1/chat/completions`, bound to
    //! `$XDG_RUNTIME_DIR/trollshell/claude-bridge.sock` — the path is not
    //! configurable.
    """
    got = anchored_paragraph_paths(fixture_lock_before, MAIN_RS_DOC_ANCHOR)
    if got != ["trollshell/claude-bridge.sock"]:
        failures.append(
            f"anchored_paragraph_paths: case C (lock file named earlier) gave {got!r}, "
            "expected only the anchor paragraph's own path"
        )

    # The anchor phrase itself is gone (the sentence was deleted or reworded):
    # fail closed, not "found nothing, therefore agree".
    try:
        anchored_paragraph_paths("//! nothing relevant in this doc at all\n", MAIN_RS_DOC_ANCHOR)
        failures.append("anchored_paragraph_paths: expected LookupError when the anchor phrase is absent")
    except LookupError:
        pass

    # The anchor phrase survives, but no path literal follows it in that
    # paragraph: also fail closed.
    fixture_no_literal = "//! One route, `POST /v1/chat/completions`, bound to nothing documented here.\n"
    try:
        anchored_paragraph_paths(fixture_no_literal, MAIN_RS_DOC_ANCHOR)
        failures.append("anchored_paragraph_paths: expected LookupError when no path literal follows the anchor")
    except LookupError:
        pass

    return failures


def self_test_unix_rs_doc() -> list[str]:
    failures = []

    # The crate's own test module deliberately dials a DIFFERENT,
    # intentionally-wrong path to exercise its error path
    # (`chat_refuses_an_unresolvable_socket_url_instead_of_falling_back`,
    # `.../trollshell/x.sock`). That plain-code string literal must never be
    # compared against the real socket name — restricting the scan to `//!`
    # lines is what excludes it.
    fixture = (
        "//! unix://$XDG_RUNTIME_DIR/trollshell/claude-bridge.sock ← the portable spelling\n"
        "fn some_test() {\n"
        '    let err = resolve_socket_path("$XDG_RUNTIME_DIR/trollshell/x.sock", None);\n'
        "}\n"
    )
    got = all_doc_paragraph_paths(fixture)
    if got != ["trollshell/claude-bridge.sock"]:
        failures.append(
            f"all_doc_paragraph_paths: expected only the `//!` line's path, got {got!r} "
            "(picked up the test fixture's plain-code literal?)"
        )

    # A `///` item-doc line is a different marker and must not leak into a
    # `//!` module-doc scan, even when it names a plausible-looking but
    # different path.
    fixture_item_doc = (
        "/// Some item doc mentions $XDG_RUNTIME_DIR/trollshell/other-thing.sock incorrectly.\n"
        "//! unix://$XDG_RUNTIME_DIR/trollshell/claude-bridge.sock ← the portable spelling\n"
    )
    got = all_doc_paragraph_paths(fixture_item_doc)
    if got != ["trollshell/claude-bridge.sock"]:
        failures.append(f"all_doc_paragraph_paths: a `///` item-doc line leaked into the `//!` scan: {got!r}")

    return failures


def self_test_nix_option_block() -> list[str]:
    failures = []

    # The decoy must be reachable by the same regex the real extraction
    # uses (a QUOTED string) and positioned in an EARLIER sibling option, so
    # an unbounded/anchor-ignoring implementation would pick it up instead.
    nix_src = """
    port = lib.mkOption {
      type = lib.types.str;
      default = "claude-bridge-old.sock";
    };

    baseUrl = lib.mkOption {
      type = lib.types.str;
      readOnly = true;
      default = "unix://$XDG_RUNTIME_DIR/trollshell/claude-bridge.sock";
    };
    """
    got = nix_option_default_str(nix_src, "baseUrl = lib.mkOption {")
    if got != "unix://$XDG_RUNTIME_DIR/trollshell/claude-bridge.sock":
        failures.append(
            f"nix_option_default_str: expected baseUrl's own default, got {got!r} "
            "(picked up the earlier `port` option's quoted default instead?)"
        )

    # Nested braces inside the option block (a submodule type, say) must not
    # make `match_delim` stop early.
    nix_src_nested = """
    baseUrl = lib.mkOption {
      type = lib.types.submodule { options = { }; };
      default = "unix://$XDG_RUNTIME_DIR/trollshell/claude-bridge.sock";
    };
    """
    got_nested = nix_option_default_str(nix_src_nested, "baseUrl = lib.mkOption {")
    if got_nested != "unix://$XDG_RUNTIME_DIR/trollshell/claude-bridge.sock":
        failures.append(f"nix_option_default_str: nested braces broke block bounding, got {got_nested!r}")

    # `baseUrl` loses its own `default` line (the exact #1099-review
    # mutation): the scan must fail closed rather than leak forward into
    # `mode`'s default four options later.
    nix_src_no_default = """
    baseUrl = lib.mkOption {
      type = lib.types.str;
      readOnly = true;
    };

    mode = lib.mkOption {
      type = lib.types.enum [ "subscription" "reprompt" "api" ];
      default = "subscription";
    };
    """
    try:
        leaked = nix_option_default_str(nix_src_no_default, "baseUrl = lib.mkOption {")
        failures.append(
            "nix_option_default_str: expected LookupError when baseUrl has no default of "
            f"its own, but got {leaked!r} (leaked into the next option's block?)"
        )
    except LookupError:
        pass

    # The anchor itself is absent.
    try:
        nix_option_block('mode = lib.mkOption { default = "x"; };', "baseUrl = lib.mkOption {")
        failures.append("nix_option_block: expected LookupError when the anchor is absent")
    except LookupError:
        pass

    return failures


def self_test() -> list[str]:
    failures = []
    for fn in (
        self_test_str_const,
        self_test_runtime_dir_token,
        self_test_main_rs_anchor,
        self_test_unix_rs_doc,
        self_test_nix_option_block,
    ):
        try:
            failures.extend(fn())
        except LookupError as e:
            failures.append(f"{fn.__name__} raised unexpectedly: {e}")

    if compose_base_url("trollshell", "claude-bridge.sock") != (
        "unix://$XDG_RUNTIME_DIR/trollshell/claude-bridge.sock"
    ):
        failures.append("compose_base_url: composition does not match the expected shape")

    return failures


def read(path: str) -> str:
    with open(path, encoding="utf-8") as fh:
        return fh.read()


def main() -> int:
    failures = self_test()
    if failures:
        print("bridge-socket scan: SELF-TEST FAILED", file=sys.stderr)
        for line in failures:
            print(f"  {line}", file=sys.stderr)
        print(
            "\nThe scanner disagrees with its own fixtures, so any verdict it gives on the\n"
            "tree is meaningless. Fix the extraction functions rather than the fixtures.",
            file=sys.stderr,
        )
        return 2

    missing = [p for p in (UNIX_RS, BRIDGE_MAIN_RS, MODULE_COMMON_NIX) if not os.path.isfile(p)]
    if missing:
        print(f"bridge-socket scan: file(s) not found: {', '.join(missing)}", file=sys.stderr)
        print("  (run from inside the repository)", file=sys.stderr)
        return 2

    unix_src = read(UNIX_RS)
    main_src = read(BRIDGE_MAIN_RS)
    nix_src = read(MODULE_COMMON_NIX)

    try:
        rust_dir = str_const(unix_src, "BRIDGE_SOCKET_DIR")
        rust_file = str_const(unix_src, "BRIDGE_SOCKET_FILE")
        rust_base_url = str_const(unix_src, "BRIDGE_BASE_URL")
        expected_suffix = f"{rust_dir}/{rust_file}"

        rust_base_url_suffixes = paths_after_runtime_dir_token(rust_base_url)
        if not rust_base_url_suffixes:
            raise LookupError("BRIDGE_BASE_URL does not name a $XDG_RUNTIME_DIR-rooted path")

        main_doc_suffixes = anchored_paragraph_paths(main_src, MAIN_RS_DOC_ANCHOR)

        unix_doc_suffixes = all_doc_paragraph_paths(unix_src)
        if not unix_doc_suffixes:
            raise LookupError("unix.rs's `//!` module doc names no $XDG_RUNTIME_DIR-rooted path")

        nix_default = nix_option_default_str(nix_src, "baseUrl = lib.mkOption {")
        nix_default_suffixes = paths_after_runtime_dir_token(nix_default)
        if not nix_default_suffixes:
            raise LookupError(
                f"claudeBridge.baseUrl's default ({nix_default!r}) does not name a "
                "$XDG_RUNTIME_DIR-rooted path"
            )

        nix_prose_suffixes = paths_after_runtime_dir_token(nix_src)
        if not nix_prose_suffixes:
            raise LookupError("nix/module-common.nix names no $XDG_RUNTIME_DIR-rooted path at all")
    except LookupError as e:
        print(f"bridge-socket scan: {e}", file=sys.stderr)
        print(
            "  (an anchor this script depends on has moved or been reworded — "
            "update nix/lint-bridge-socket.py to match)",
            file=sys.stderr,
        )
        return 2

    mismatches = []

    def check(label: str, values: list[str]) -> None:
        bad = sorted({v for v in values if v != expected_suffix})
        if bad:
            mismatches.append(f"{label}: found {bad!r}, expected {expected_suffix!r}")

    check(
        "hytte-ai-providers self-consistency (BRIDGE_BASE_URL vs BRIDGE_SOCKET_DIR/BRIDGE_SOCKET_FILE)",
        rust_base_url_suffixes,
    )
    check("hytte-claude-bridge doc drift (main.rs's module doc)", main_doc_suffixes)
    check("hytte-ai-providers doc drift (unix.rs's module doc worked example)", unix_doc_suffixes)
    check("nix drift (claudeBridge.baseUrl's own default)", nix_default_suffixes)
    check("nix drift (module-common.nix prose)", nix_prose_suffixes)

    if mismatches:
        print(f"\nERROR: {len(mismatches)} claude-bridge socket name mismatch(es):\n", file=sys.stderr)
        for line in mismatches:
            print(f"  - {line}", file=sys.stderr)
        print(
            "\nThe bridge socket path is spelled out in nix/module-common.nix and in prose in\n"
            "both crates/hytte-ai-providers/src/unix.rs and "
            "crates/hytte-claude-bridge/src/main.rs —\nnone of that is compiler-checked "
            "against BRIDGE_SOCKET_DIR/BRIDGE_SOCKET_FILE. Update\nwhichever side fell "
            "behind so the daemon binds the path its clients actually dial.",
            file=sys.stderr,
        )
        return 1

    print(
        f"bridge-socket scan: {compose_base_url(rust_dir, rust_file)!r} — nix, "
        "both crates' docs, and the Rust constants all agree",
        flush=True,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
