#!/usr/bin/env python3
"""Fail if a `bind*` call site pins its widget with a captured strong clone.

THE DEFECT
----------
`hytte_reactive::bind` (and its `bind_text`/`bind_visible`/`bind_class`/
`bind_two_way*` siblings) hold their widget through a `glib::WeakRef` and hand
it back to the apply closure as the closure's first parameter — see the
contract at `crates/hytte-reactive/src/bind.rs:16-22`. That weak hold is the
whole point: when a bar or drawer is torn down on monitor hot-plug, the next
emission upgrades to `None` and the binding releases itself (#224).

The shape that defeats it:

    let w_for_bind = w.clone();                 // a *strong* ref
    bind(some_signal(), &w, move |_w, value| {  // own parameter discarded
        w_for_bind.set_something(value);        // ... clone used instead
    });

The clone is moved into the apply closure, so the closure — and therefore the
binding — owns a strong ref to the widget for its own lifetime. The widget
tree stays alive until the *signal* ends, which for a service accessor is
never. Nothing misbehaves visibly; the cost is a leak on teardown, which is
exactly what the `WeakRef` exists to prevent.

#772 fixed four such sites and closed on an inventory of four. #831 found
twelve. #834 fixed all twelve. This script is what made that count checkable
rather than a number to trust, so it lives in the tree and runs in CI instead
of being re-derived by hand every time someone wonders.

THE CARVE-OUT (do not regress this)
-----------------------------------
Cloning a widget into a `bind` closure is only a defect when the clone is
*the same widget* `bind` was given. A closure routinely needs a **second**
widget that the apply parameter cannot supply, and capturing that one is
correct, not a pin:

  - `trollshell/src/panels/network/traffic.rs`  `idle_expander_for_bind`
    (the idle bucket; `bind`'s target is the sibling `iface_group`)
  - `trollshell/src/panels/audio/playback.rs`   `placeholder_for_bind`
    (a `ListBoxRow` child; `bind`'s target is the `ListBox`)

Both are live on `main` and both must stay unflagged. Two independent things
keep them clean here: the clone's *source* identifier is not `bind`'s target
(so it never enters the alias set), and the closure uses its own parameter
(so it is not a discard). A change that starts flagging either one is a bug in
this script, not a finding.

WHY A PARSER-ISH SCAN AND NOT A REGEX
-------------------------------------
This is the part worth preserving. The obvious implementation — "match
`let X = W.clone();` and a `bind(…, &W, move |_` within N lines" — was tried,
and it is *silently* wrong:

  * **A line window truncates.** A six-line window over `main` found nine of
    the twelve. The three it missed (`panels/audio/playback.rs`,
    `panels/notifications.rs`, `widgets/disk.rs`) sit further from their clone
    or wrap their closure header differently. A guard that finds three
    quarters of the defect and reports success is worse than no guard: it
    would have "confirmed" #772's inventory of four.

  * **Argument lists nest.** `bind(pipewire::playback_streams(), &list, move
    |list, streams: Vec<PlaybackStream>| { … })` contains parens inside the
    signal expression, inside the type annotation, and all through the body.
    Finding where the call ends needs a depth counter, not `[^)]*`.

  * **Closure bodies nest harder.** The body is where the clone has to be
    looked for, and delimiting it means brace-matching — bodies contain
    `match` arms, nested closures, and struct literals.

  * **Comments and string literals lie.** A `bind` in a doc comment or a
    `.clone()` inside a format string would both produce phantom hits, so
    literal and comment *contents* are blanked (length-preserving, so byte
    offsets still map back to real line numbers).

  * **The clone can be several hops away.** `let a = w.clone(); let b =
    a.clone();` — so the alias set is closed transitively rather than matched
    once.

So: blank the noise, paren-match each `bind*` call's full argument text,
locate `&<ident>, (move)? |params|` inside it, brace-match the closure body,
then walk everything *before* the call for `let X = <alias>.clone()`. That is
not a full Rust parse — it does not need to be — but every shortcut above was
removed because it demonstrably lost hits.

USAGE
-----
    python3 nix/lint-bind-pins.py [ROOT ...]      # from the repo root

Exits 0 when clean, 1 when a pin is found (naming every site), 2 when the scan
itself is untrustworthy (a root has gone missing, or too few call sites were
seen to believe the result).
"""

import os
import re
import sys

# The three trees the #831 audit covered. `trollshell/src` is where all twelve
# hits lived; the other two came back empty and are scanned precisely so that
# stays true — a guard is for the code that is currently clean.
DEFAULT_ROOTS = (
    "trollshell/src",
    "crates/trollshell-control-center/src",
    "crates/hytte-ui/src",
)

# Longest-first so the alternation prefers `bind_two_way` over `bind`; `\b`
# alone would let `bind` match the head of `bind_text` and mislabel the site.
BIND_FNS = (
    "bind_two_way_drag_safe",
    "bind_two_way",
    "bind_class",
    "bind_visible",
    "bind_text",
    "bind",
)

# Anti-vacuity floor. If `bind` is ever renamed, re-exported under a new name,
# or this file's call regex is broken by a refactor, the honest failure mode is
# "0 sites scanned, 0 hits, exit 0" — a permanently green check that guards
# nothing. There were 157 call sites across the three roots when this was
# written; the floor sits far enough below that ordinary churn never trips it
# and far enough above zero that a broken scan cannot pass.
MIN_CALL_SITES = 100

IDENT = r"[A-Za-z_][A-Za-z0-9_]*"


def blank_noise(src: str) -> str:
    """Blank comment and string/char *contents*, preserving byte offsets.

    Offsets are preserved (spaces substituted one-for-one) so a match position
    in the cleaned text still maps to the right line in the original file.
    """
    out = list(src)
    i, n = 0, len(src)
    while i < n:
        c = src[i]
        if c == "/" and i + 1 < n and src[i + 1] == "/":
            j = src.find("\n", i)
            j = n if j < 0 else j
            out[i:j] = " " * (j - i)
            i = j
        elif c == "/" and i + 1 < n and src[i + 1] == "*":
            j = src.find("*/", i + 2)
            j = n if j < 0 else j + 2
            out[i:j] = " " * (j - i)
            i = j
        elif c == '"':
            j = i + 1
            while j < n:
                if src[j] == "\\":
                    j += 2
                    continue
                if src[j] == '"':
                    break
                j += 1
            j = min(j, n)
            out[i + 1 : j] = " " * max(0, j - i - 1)
            i = j + 1
        elif c == "'":
            # Only a genuine char literal; a lone `'` is a lifetime tick.
            m = re.match(r"'(\\.|[^\\'])'", src[i:])
            if m:
                out[i + 1 : i + m.end() - 1] = " " * (m.end() - 2)
                i += m.end()
            else:
                i += 1
        else:
            i += 1
    return "".join(out)


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


def clone_aliases(prefix: str, target: str) -> set[str]:
    """Every name that is `target` by a chain of `.clone()`s, `target` included.

    Transitive because `let a = w.clone(); let b = a.clone();` is still `w`.
    Iterates to a fixed point rather than assuming declaration order.
    """
    aliases = {target}
    pattern = re.compile(rf"let\s+({IDENT})\s*(?::[^=;]*)?=\s*({IDENT})\s*\.\s*clone\s*\(\s*\)")
    while True:
        grew = False
        for m in pattern.finditer(prefix):
            if m.group(2) in aliases and m.group(1) not in aliases:
                aliases.add(m.group(1))
                grew = True
        if not grew:
            return aliases


def scan_file(path: str, src: str, hits: list) -> int:
    """Append pin hits found in `src`; return the number of call sites seen."""
    clean = blank_noise(src)
    call_sites = 0

    for m in re.finditer(rf"\b({'|'.join(BIND_FNS)})\s*\(", clean):
        # Skip the definitions themselves (`pub fn bind(` in hytte-reactive)
        # and any `use …::bind(`-shaped import.
        if re.search(r"(fn|use)\s+$", clean[max(0, m.start() - 40) : m.start()]):
            continue
        open_paren = clean.index("(", m.start())
        end = match_delim(clean, open_paren, "(", ")")
        if end < 0:
            continue
        call_sites += 1
        args = clean[open_paren + 1 : end - 1]
        base = open_paren + 1

        # `&<widget>, (move)? |params|` — the widget argument immediately
        # followed by the apply closure, whichever argument position it sits in.
        for am in re.finditer(rf"&\s*({IDENT})\s*,\s*(move\s+)?\|([^|]*)\|", args):
            target = am.group(1)
            first_param = (am.group(3).split(",") or [""])[0].strip()
            pm = re.match(rf"^(?:mut\s+)?({IDENT})", first_param)
            param_name = pm.group(1) if pm else ""

            # Only `_`-prefixed first params are discards. A closure that uses
            # its own widget parameter is correct by construction, and a clone
            # alongside it is the second-widget carve-out.
            if not param_name.startswith("_"):
                continue

            after = args[am.end() :]
            lead = after.lstrip()
            if lead.startswith("{"):
                bstart = am.end() + (len(after) - len(lead))
                bend = match_delim(args, bstart, "{", "}")
                body = args[bstart : bend if bend > 0 else len(args)]
            else:
                # Expression-bodied closure: runs to the end of this call.
                body = after

            prefix = clean[: base + am.start()]
            used = sorted(
                a
                for a in clone_aliases(prefix, target)
                if a != target and re.search(rf"\b{re.escape(a)}\b", body)
            )
            if used:
                line = src[: base + am.start()].count("\n") + 1
                hits.append((path, line, m.group(1), target, param_name, used))

    return call_sites


def main(argv: list[str]) -> int:
    roots = argv[1:] or list(DEFAULT_ROOTS)

    # A missing root would make `os.walk` yield nothing and the scan pass
    # vacuously. Renaming a scanned tree must turn this check red, not green.
    missing = [r for r in roots if not os.path.isdir(r)]
    if missing:
        print(f"bind-pin scan: root(s) not found: {', '.join(missing)}", file=sys.stderr)
        print("  (run from the repository root, or pass roots explicitly)", file=sys.stderr)
        return 2

    hits: list = []
    call_sites = 0
    files = 0
    for root in roots:
        for dirpath, _, names in os.walk(root):
            for name in sorted(names):
                if not name.endswith(".rs"):
                    continue
                path = os.path.join(dirpath, name)
                with open(path, encoding="utf-8") as fh:
                    src = fh.read()
                files += 1
                call_sites += scan_file(path, src, hits)

    # Flushed so the summary lands *before* the stderr report below when both
    # are funnelled into one build log.
    print(
        f"bind-pin scan: {files} files, {call_sites} bind* call sites, {len(hits)} pin(s)",
        flush=True,
    )

    if call_sites < MIN_CALL_SITES:
        print(
            f"\nERROR: only {call_sites} bind* call sites seen, expected at least "
            f"{MIN_CALL_SITES}.\nThe scan is not trustworthy — `bind` was likely renamed or the "
            "roots\nno longer hold the shell's widget code. Fix this script (BIND_FNS /\n"
            "DEFAULT_ROOTS / MIN_CALL_SITES) rather than lowering the floor to pass.",
            file=sys.stderr,
        )
        return 2

    if not hits:
        return 0

    print(
        f"\nERROR: {len(hits)} bind* call site(s) pin their widget with a captured "
        "strong clone:\n",
        file=sys.stderr,
    )
    for path, line, fn, target, param, used in hits:
        clones = ", ".join(used)
        print(f"  {path}:{line}", file=sys.stderr)
        print(
            f"      {fn}(.., &{target}, move |{param}, ..|)  captures: {clones}",
            file=sys.stderr,
        )
    print(
        "\nEach of these discards the closure's own widget parameter and uses a strong\n"
        "clone of the same widget instead, so the binding keeps the widget alive for its\n"
        "own lifetime — defeating the WeakRef contract in\n"
        "crates/hytte-reactive/src/bind.rs:16-22 (#224, #772, #831).\n\n"
        "Fix: drop the `let <name>_for_bind = <widget>.clone();` and use the closure's\n"
        "parameter (`move |widget, value|`) in the body.\n\n"
        "If the captured widget is genuinely a *different* widget from bind's target,\n"
        "this script should not have flagged it — see the carve-out section in\n"
        "nix/lint-bind-pins.py.",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
