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

THE SITE SHAPE — WHY THE `&` IS OPTIONAL (#973)
-----------------------------------------------
`bind` takes `impl IsA<Widget>`, so a call site spells its widget argument one
of two ways, and the check has to see both:

    bind(sig, &container, move |container, v| …)   // a local, borrowed here
    bind(sig, container,  move |container, v| …)   // already a `&W` parameter

The second is the *extraction* shape (#772, #972): a builder's `bind` call is
split into `fn bind_rows(container: &W, signal: S)` so the binding can be
driven with a synthetic signal in a test, and the parameter is passed straight
through — `W: IsA<Widget> + Clone` rules out re-borrowing, so there is no
sigil to match on. Until #973 this script required a leading `&`, so every
extracted helper sat outside its coverage: a strong clone reintroduced inside
one scanned green, and the two guards traded off exactly where they should
have overlapped. The probe below is that case.

Widening to a bare identifier was measured before it was taken, because a
wider match is only worth having if the rows it adds are clean. Across the
three roots it surfaces **24 further argument-matches**:

  * **13 are a path tail** and are rejected outright. `bind_two_way(sig, &sw,
    gtk::Switch::set_active, |w| …)` also puts an identifier immediately
    before a closure — a `::`-preceded identifier is never a local widget
    binding, hence the `(?<!:)` in `SITE_RE`. A self-test fixture pins this,
    because without the rejection that shape *is* reported.
  * **8 use the closure's own widget parameter** and so can never be flagged
    (the `param_name.startswith("_")` gate below).
  * **3 discard it**, and all three are the *anchor idiom* — `bind` used
    purely to tie a subscription's lifetime to a widget the closure never
    touches: `components/open_refresh.rs` (`on_open`), `widgets/calendar.rs`
    (`wire_clock_bind`), `widgets/tasks.rs` (`wire_lists_bind`). None clones
    its anchor, so none is a hit, and none can become one without someone
    writing the defect.
  * **0 new pins.** The tree is still `0 pin(s)` after the widening.

All eleven surviving rows are a `&W` parameter of the enclosing function, but
they are *not* all named `bind_*` — `wire_events_bind`, `wire_clock_bind`,
`wire_tasks_bind`, `wire_lists_bind`, `wire_visibility_and_state`, `on_open`
and `reactive_list` take the same shape under other names. A narrower rule
keyed on the helper's *name* would therefore have missed live sites, and a rule
keyed on the enclosing function's parameter types would fail *silently*
(under-reporting) — the same failure direction #973 is a report of. A bare
identifier plus the existing discard gate fails loudly instead, which is the
tolerable direction for a lint.

`panels/connections.rs`'s `bind_connections_group` is worth knowing about: it
is the carve-out above *at a bare-parameter site* — target `conn_group`, with
`other_expander.clone()` captured for a genuinely different widget — and the
widening leaves it unflagged for both of the reasons in that section.

THE PROBE
---------
The regression this file exists to prevent is checked on every run by
`self_test()` below, but the end-to-end version is worth keeping by hand:
reintroduce #834's exact defect shape (`git show 8cb1ddd --
trollshell/src/widgets/tray.rs`) *inside* a helper, e.g. in
`trollshell/src/panels/vpn.rs`'s `bind_tunnel_groups`:

    let groups: … = …;
    let column_for_bind = column.clone();          // + a strong clone
    bind(signal, column, move |_column, tunnels| { // + a discarded parameter
        …  column_for_bind.remove(&g);  …          // + the clone used instead
    });

then run the scanner. Before #973 that reported `0 pin(s)`, exit 0, and adding
a single `&` at the call site — nothing else changed — flipped it to
`1 pin(s)`. It now reports the pin either way. Revert with `git checkout --`.

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
itself is untrustworthy — a root has gone missing, too few call sites were seen
to believe the result, or `self_test()` (which runs first, on every invocation,
against the fixtures at the bottom of this file) disagrees with the scanner.
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

# The widget argument sitting immediately before the apply closure. Groups:
# 1 the optional `&` sigil, 2 the widget identifier, 3 an optional `move`,
# 4 the closure's parameter list. Rationale for the optional sigil and the
# `::` rejection is at the use site in `scan_file`, and in "THE SITE SHAPE".
SITE_RE = rf"(&\s*)?(?<!:)\b({IDENT})\s*,\s*(move\s+)?\|([^|]*)\|"


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

        # `[&]<widget>, (move)? |params|` — the widget argument immediately
        # followed by the apply closure, whichever argument position it sits in.
        #
        # The `&` is optional (#973): an extracted `bind_*(widget: &W, signal)`
        # helper passes its own parameter through with no sigil, and requiring
        # one put every such site outside this check. See "THE SITE SHAPE"
        # above for the measured cost of the widening.
        #
        # `(?<!:)` rejects a path tail — `bind_two_way(sig, &sw,
        # gtk::Switch::set_active, |w| …)` also puts an identifier immediately
        # before a closure, and it is never a local widget binding.
        for am in re.finditer(SITE_RE, args):
            sigil = "&" if am.group(1) else ""
            target = am.group(2)
            first_param = (am.group(4).split(",") or [""])[0].strip()
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
                hits.append((path, line, m.group(1), sigil, target, param_name, used))

    return call_sites


# Fixtures for `self_test()`, run on every invocation. Each is a snippet of
# Rust and the number of pins the scanner must find in it. They exist because
# this check spent months reporting green over a shape it could not see (#973):
# the tree being clean proves nothing about whether the scanner still *works*,
# and only a case that is deliberately dirty can tell the two apart.
#
# Every fixture is a real shape from the tree, not an invention:
#   the amp / helper pins   #834's defect, at both call-site spellings
#   the two carve-outs      traffic.rs's `idle_expander_for_bind` and
#                           playback.rs's `placeholder_for_bind`
#   the anchor idiom        open_refresh.rs / tasks.rs / calendar.rs
#   the path tail           bind_two_way's setter argument
SELF_TEST_CASES: tuple[tuple[str, int, str], ...] = (
    (
        "amp pin (#834 shape, borrowed local)",
        1,
        """
        fn build() {
            let container = gtk::Box::new();
            let container_for_signal = container.clone();
            bind(tray::items(), &container, move |_, items| {
                update_tray(&container_for_signal, &items);
            });
        }
        """,
    ),
    (
        "helper pin (#973 shape, `&W` parameter passed bare)",
        1,
        """
        fn bind_tunnel_groups<S>(column: &gtk::Box, signal: S) {
            let column_for_bind = column.clone();
            bind(signal, column, move |_column, tunnels| {
                column_for_bind.remove(&tunnels);
            });
        }
        """,
    ),
    (
        "helper pin reached through a transitive clone chain",
        1,
        """
        fn bind_rows<S>(group: &adw::PreferencesGroup, signal: S) {
            let a = group.clone();
            let b = a.clone();
            bind(signal, group, move |_g, rows| {
                b.set_rows(rows);
            });
        }
        """,
    ),
    (
        "helper using its own parameter (the fix; must stay clean)",
        0,
        """
        fn bind_tunnel_groups<S>(column: &gtk::Box, signal: S) {
            bind(signal, column, move |column, tunnels| {
                column.remove(&tunnels);
            });
        }
        """,
    ),
    (
        "second-widget carve-out at a bare-parameter site (must stay clean)",
        0,
        """
        fn bind_iface_rows<S>(iface_group: &adw::PreferencesGroup, signal: S) {
            let idle_expander = build_idle_expander();
            let idle_expander_for_bind = idle_expander.clone();
            bind(signal, iface_group, move |iface_group, links| {
                iface_group.set_rows(&links);
                idle_expander_for_bind.set_visible(!links.is_empty());
            });
        }
        """,
    ),
    (
        "second-widget carve-out with a discarded parameter (must stay clean)",
        0,
        """
        fn bind_placeholder<S>(list: &gtk::ListBox, signal: S) {
            let placeholder = build_placeholder();
            let placeholder_for_bind = placeholder.clone();
            bind(signal, list, move |_list, streams| {
                placeholder_for_bind.set_visible(streams.is_empty());
            });
        }
        """,
    ),
    (
        "anchor idiom — bare target, discarded parameter, no clone",
        0,
        """
        fn on_open<W>(monitor: &Monitor, anchor: &W, refresh: impl Fn()) {
            bind(sidebar::open_signal(monitor), anchor, move |_, open| {
                if open {
                    refresh();
                }
            });
        }
        """,
    ),
    (
        "path tail — `::`-qualified setter must never become the target",
        0,
        """
        fn build() {
            let set_active = gtk::Switch::new();
            let set_active_clone = set_active.clone();
            bind_two_way(dnd::enabled(), &dnd_switch, gtk::Switch::set_active, |_w| {
                set_active_clone.is_active()
            });
        }
        """,
    ),
    (
        "comment and string contents must not produce phantom hits",
        0,
        """
        fn build() {
            let container = gtk::Box::new();
            let container_for_bind = container.clone();
            // bind(sig, container, move |_c, v| { container_for_bind.set(v); });
            let doc = "bind(sig, container, move |_c, v| container_for_bind)";
            container.set_tooltip(doc);
        }
        """,
    ),
)


def self_test() -> list[str]:
    """Run the scanner over the fixtures; return a list of failure lines."""
    failures = []
    for name, expected, src in SELF_TEST_CASES:
        hits: list = []
        scan_file("<self-test>", src, hits)
        if len(hits) != expected:
            found = ", ".join(f"{t} captures {u}" for _, _, _, _, t, _, u in hits) or "nothing"
            failures.append(f"  {name}: expected {expected} pin(s), found {len(hits)} ({found})")
    return failures


def main(argv: list[str]) -> int:
    # Before anything else: does the scanner still find a pin it is supposed to
    # find, and still ignore the shapes it is supposed to ignore? A clean tree
    # cannot answer either question, which is how #973 stayed green.
    failures = self_test()
    if failures:
        print("bind-pin scan: SELF-TEST FAILED", file=sys.stderr)
        for line in failures:
            print(line, file=sys.stderr)
        print(
            "\nThe scanner disagrees with its own fixtures, so any verdict it gives on the\n"
            "tree is meaningless. Fix scan_file()/SITE_RE rather than the expectations —\n"
            "and if a fixture is genuinely wrong, say why in the header section it cites.",
            file=sys.stderr,
        )
        return 2

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
    for path, line, fn, sigil, target, param, used in hits:
        clones = ", ".join(used)
        print(f"  {path}:{line}", file=sys.stderr)
        print(
            f"      {fn}(.., {sigil}{target}, move |{param}, ..|)  captures: {clones}",
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
