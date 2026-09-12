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

THE ANCESTOR SHAPE (#1176) — rule (a)
-------------------------------------
Everything above is about `bind`. The same contract is defeated by a shape
with no `bind` in it at all, which is why the #1162 sweep found four live
cycles this file was structurally blind to:

    let popover = gtk::Popover::new();
    let pop_box  = gtk::Box::new(…);
    let btn      = gtk::Button::with_label("Forget");
    let pop_for_btn = popover.clone();            // a *strong* ref
    btn.connect_clicked(move |_| { … pop_for_btn.popdown(); });
    pop_box.append(&btn);                         // btn is INSIDE popover
    popover.set_child(Some(&pop_box));

`popover → pop_box → btn → handler → popover` is a refcount cycle, and GTK
breaks none: the popover, its box, its buttons and anything else they hold
outlive the row that built them. `bind`'s `WeakRef` is worth nothing if the
subtree underneath it cannot be freed — and the rows carrying these menus are
rebuilt on every service emission, so an active Bluetooth scan or Wi-Fi rescan
leaked one menu subtree per device *per emission*, no click needed.

So rule (a): a `connect_*` closure that captures a strong clone of one of the
receiver widget's own **ancestors**. Ancestry is read out of the same
statements GTK reads — `parent.append(&child)`, `set_child`, `attach`,
`add_prefix/add_suffix`, `set_popover`, … (`CONTAINER_METHODS`) — scoped to the
enclosing function, because an ancestry claim assembled from two unrelated
functions' plumbing would be a fabrication. Only edges whose child argument is
a bare local count (`&build_device_menu(dev, …)` and `&cell.button` contribute
nothing), so the graph *under*-reports rather than inventing parents.

Two things the rule needs to be worth having, both measured on the tree:

  * **The indirection through a named closure.** `widgets/tasks.rs` builds
    `let do_create = move || { … popover_for_create.popdown(); … };` and then
    installs it with `create.connect_clicked(move |_| do_create_for_button())`
    — the handler's own body does not mention the clone at all. So a named
    closure's body is folded into any handler that (transitively, through
    `.clone()` aliases) calls it. Without that fold, two of the four #1176
    sites report clean.
  * **The carve-out survives.** A handler capturing a *different* widget —
    a sibling, a child, anything not on its own parent chain — is correct and
    stays unflagged. `panels/bluetooth.rs`'s `submit_btn.connect_clicked(move
    |_| submit_entry(&entry_for_submit, …))` is exactly that: the entry is the
    button's sibling, so it closes no cycle and is not reported, while the
    *entry's own* `connect_activate` capturing the same clone was a cycle.

Measured across the three roots when the rule landed: **205** `connect_*`
handlers with a closure argument, **12** of them pinning — the four sites
#1176 names, plus nine more of the identical popover-menu shape in
`panels/vpn.rs`, `panels/network/wifi.rs` and `panels/network/wired.rs` that
nobody had looked at, all fixed in the same PR, and one in
`crates/hytte-ui/src/popup.rs` (below). Zero false positives.

What it misses, measured the same way by reverting all of #1176's fixes and
re-running: **`open_edit_popover`'s three popdown handles in
`widgets/tasks.rs`**. Its popover comes out of `hytte::ui::Popup`'s *builder*
— `Popup::new(parent).child(column).build()` — so the `popover → column` edge
is never spelled as `popover.set_child(&column)` and the graph has no path
from those buttons up to the popover. Modelling a builder chain means
resolving what the chain's terminal `build()` was bound to, which is a
different kind of analysis from reading one method call, so it is not done:
those three are fixed by hand and documented at the site. This is the
under-reporting direction on purpose — a lint that invents an ancestor would
be worse than one that misses a builder — but it means "0 pin(s)" is not the
same claim as "no cycles in the tree", and a reviewer of a *new* popover built
through `Popup` has to check it by reading.

THE SELF CASE IS NOT COVERED (YET)
----------------------------------
`ancestors_of` deliberately excludes the receiver itself, so

    entry.connect_activate(move |_| submit_entry(&entry_for_activate, …));

— a widget cloned into its *own* handler, which pins it through its own
handler list — is **not** reported. It is the same defect (and the same fix:
take the handler's own widget argument, which `panels/bluetooth.rs` now does),
and switching `ancestors_of` to include the node would find it. The reason it
does not is timing, not principle: the only other instance in the tree is
`attach_dismiss_catcher` in `crates/hytte-ui/src/popup.rs`, which captures a
strong `popover.clone()` inside the popover's own `connect_show`, and that site
is owned by #1180. Turning the self case on before #1180 lands would make
`main` red on a file this PR must not touch. There is a self-test fixture below
pinning the current (uncovered) behaviour, so flipping the rule fails it loudly
and whoever flips it updates the expectation on purpose.

THE FIELD SHAPE (#1176) — rule (b)
----------------------------------
The second blind spot is a `bind` site after all, but one whose capture is not
an identifier the alias scan could ever see:

    let w = w.clone();                 // an InfoWidgets, i.e. a *holder*
    let title = w.title.clone();       // bind's target is a FIELD of it
    bind(mpris::active_player(), &title, move |_, player| {
        render_player(&w, …);          // holding `w` holds `title`
    });

Holding the holder holds the target, so the weak upgrade can never fail and
the apply-loop never ends. `#[derive(Clone)]` widget-holder structs are how
this tree spells "the handles this closure needs", which makes the shape
likely rather than exotic.

Rule (b) therefore closes the alias set over one more relation: `let <target> =
<holder>.<field>.clone()` makes `<holder>` (and every clone of it) a name that
pins `<target>`. Unlike rule (a) and the original rule there is **no
carve-out** and no discard gate: a closure may take its own `title` parameter
*and* capture the `InfoWidgets` whose first field is that same label, and the
pin is just as total. Which field the body actually reads is irrelevant.

One site on the tree (`panels/media.rs`, fixed in the same PR); the fix is
#834's, move the field out of the holder and take it from the closure's own
argument.

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

# The same anti-vacuity floor for the `connect_*` scan added by #1176. There
# were 205 `connect_*` handlers with a closure argument across the three roots
# when it was written (of 222 `.connect_*(` occurrences — the rest are
# field/method receivers like `self.calendar.connect_day_selected`, which
# CONNECT_RE rejects because they are not local bindings this scan can resolve
# a clone against). The floor sits far enough below 205 that ordinary churn
# never trips it and far enough above zero that a broken scan cannot pass.
MIN_CONNECT_SITES = 150

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


def owner_aliases(prefix: str, target: str) -> set[str]:
    """Every name whose *field* `target` is, transitively through `.clone()`s.

    Rule (b) of #1176. `let title = w.title.clone();` means a closure that
    captures `w` is holding `title` — the struct owns the field, so pinning
    the struct pins the widget just as surely as cloning it directly, and
    `#[derive(Clone)]` widget-holder structs are how this tree spells "the
    handles this closure needs".

    Unlike [`clone_aliases`] there is no carve-out: holding the owner *is*
    holding the target, whichever field the body actually reads. Every name
    that is an owner by a chain of `.clone()`s counts too, so
    `let w = holder.clone(); let title = w.title.clone();` reports both.
    """
    owners: set[str] = set()
    pattern = re.compile(
        rf"let\s+({IDENT})\s*(?::[^=;]*)?=\s*({IDENT})"
        rf"((?:\s*\.\s*(?!clone\s*\()(?:{IDENT}))+)\s*\.\s*clone\s*\(\s*\)"
    )
    for m in pattern.finditer(prefix):
        if m.group(1) == target:
            owners |= clone_aliases(prefix, m.group(2))
    return owners - {target}


# ── Rule (a): a `connect_*` handler that strong-clones its own widget ────────
#
# See "THE ANCESTOR SHAPE (#1176)" in the header. The containment graph below
# is what turns "is this widget an ancestor of that one" from a guess into a
# read of the same statements GTK reads.

# Calls that make their first widget argument a child of the receiver. Only
# methods whose *first* argument is the child are listed: `grid.attach(&w, c,
# r, 1, 1)` qualifies, `box_.insert_child_after(&w, Some(&sibling))` qualifies
# on its first argument alone, and anything whose child is in a later position
# is simply not modelled (a missed edge under-reports, which is the safe
# direction for an ancestry claim).
CONTAINER_METHODS = (
    "append",
    "prepend",
    "attach",
    "add_prefix",
    "add_suffix",
    "add_row",
    "add_overlay",
    "add_titled",
    "add_named",
    "add_child",
    "add_toplevel",
    "insert_child_after",
    "pack_start",
    "pack_end",
    "set_child",
    "set_content",
    "set_popover",
    "set_extra_child",
    "set_header_suffix",
    "set_start_widget",
    "set_end_widget",
    "set_title_widget",
    "set_titlebar",
    "set_popup",
)

CONTAIN_RE = re.compile(
    rf"(?<![.\w:])({IDENT})\s*\.\s*(?:{'|'.join(CONTAINER_METHODS)})\s*\("
)

# `<local>.connect_<signal>(`. The lookbehind rejects a field/method receiver
# (`self.calendar.connect_day_selected`) and a path tail, neither of which is a
# local binding this scan can reason about.
CONNECT_RE = re.compile(rf"(?<![.\w:])({IDENT})\s*\.\s*(connect_[A-Za-z0-9_]+)\s*\(")

# A `let` whose value is a closure — `let do_create = move || { … };`. Such a
# closure is routinely *installed* by a `connect_*` one or two statements
# later (`create.connect_clicked(move |_| do_create_for_button())`), so its
# body has to be folded into the handler's before the capture check runs, or
# the shape hides behind the indirection.
NAMED_CLOSURE_RE = re.compile(rf"let\s+({IDENT})\s*(?::[^=;]*)?=\s*(?:move\s+)?\|")

# A function item at any indent. Ancestry and alias resolution are scoped to
# one function body: unlike the `bind` scan (which walks the whole file prefix
# for `.clone()`s and cannot be widened now without changing what it reports),
# an ancestry claim assembled from two unrelated functions' plumbing would be
# a fabrication.
FN_RE = re.compile(
    rf"(?m)^[ \t]*(?:pub(?:\s*\([^)]*\))?\s+)?(?:const\s+)?(?:async\s+)?"
    rf"(?:unsafe\s+)?(?:extern\s+\"[^\"]*\"\s+)?fn\s+{IDENT}"
)


def iter_function_bodies(clean: str):
    """Yield `(start, end)` spans of every function body in `clean`."""
    for m in FN_RE.finditer(clean):
        brace = clean.find("{", m.end())
        if brace < 0:
            continue
        # A declaration without a body (`fn f(&self);` in a trait) would
        # otherwise borrow the *next* function's brace.
        if ";" in clean[m.end() : brace]:
            continue
        end = match_delim(clean, brace, "{", "}")
        if end < 0:
            continue
        yield brace + 1, end - 1


def first_ident_arg(args: str) -> str | None:
    """The plain local name in `&x` / `Some(&x)` / `x`, else `None`.

    Anything that is not a bare local — a constructor call
    (`&build_device_menu(dev, is_busy)`), a field path (`&cell.button`), a
    `::`-qualified path — yields `None`, so it contributes no containment
    edge. Under-reporting is the safe direction here.
    """
    s = args.lstrip()
    m = re.match(r"Some\s*\(", s)
    if m:
        s = s[m.end() :].lstrip()
    s = s.lstrip("&").lstrip()
    m = re.match(rf"({IDENT})", s)
    if not m:
        return None
    rest = s[m.end() :].lstrip()
    if rest[:1] in ("(", ".", ":"):
        return None
    return m.group(1)


def containment_edges(body: str) -> dict[str, set[str]]:
    """`child -> {parents}` for every `parent.append(&child)`-shaped call."""
    edges: dict[str, set[str]] = {}
    for m in CONTAIN_RE.finditer(body):
        open_paren = m.end() - 1
        end = match_delim(body, open_paren, "(", ")")
        if end < 0:
            continue
        child = first_ident_arg(body[open_paren + 1 : end - 1])
        if child and child != m.group(1):
            edges.setdefault(child, set()).add(m.group(1))
    return edges


def ancestors_of(edges: dict[str, set[str]], node: str) -> set[str]:
    """Every transitive parent of `node` — **proper** ancestors only.

    `node` itself is deliberately excluded; see "THE SELF CASE IS NOT COVERED
    (YET)" in the header for why, and for the one site in this tree that
    would otherwise be reported.
    """
    seen: set[str] = set()
    stack = [node]
    while stack:
        for parent in edges.get(stack.pop(), ()):
            if parent not in seen:
                seen.add(parent)
                stack.append(parent)
    return seen - {node}


def closure_body(text: str, bar: int) -> str:
    """The body of the closure whose parameter list opens at `text[bar]`."""
    close = text.find("|", bar + 1)
    if close < 0:
        return ""
    after = text[close + 1 :]
    lead = after.lstrip()
    start = close + 1 + (len(after) - len(lead))
    if lead.startswith("{"):
        end = match_delim(text, start, "{", "}")
        return text[start : end if end > 0 else len(text)]
    # Expression-bodied: runs to the end of the enclosing statement.
    depth = 0
    for i in range(start, len(text)):
        c = text[i]
        if c in "([{":
            depth += 1
        elif c in ")]}":
            if depth == 0:
                return text[start:i]
            depth -= 1
        elif c == ";" and depth == 0:
            return text[start:i]
    return text[start:]


def named_closures(body: str) -> dict[str, str]:
    """`name -> body` for every `let <name> = (move)? |…| …;` in `body`."""
    out: dict[str, str] = {}
    for m in NAMED_CLOSURE_RE.finditer(body):
        bar = body.find("|", m.end() - 1)
        if bar >= 0:
            out[m.group(1)] = closure_body(body, bar)
    return out


def expand_named_closures(handler: str, closures: dict[str, str], scope: str) -> str:
    """`handler` plus the body of every named closure it (transitively) calls."""
    text = handler
    folded: set[str] = set()
    while True:
        grew = False
        for name, cbody in closures.items():
            if name in folded:
                continue
            if any(
                re.search(rf"\b{re.escape(alias)}\b", text)
                for alias in clone_aliases(scope, name)
            ):
                text += "\n" + cbody
                folded.add(name)
                grew = True
        if not grew:
            return text


def scan_connect_pins(path: str, src: str, hits: list) -> int:
    """Append ancestor-pin hits; return the number of `connect_*` sites seen."""
    clean = blank_noise(src)
    sites = 0
    seen: set[tuple] = set()

    for fn_start, fn_end in iter_function_bodies(clean):
        body = clean[fn_start:fn_end]
        edges = containment_edges(body)
        closures = named_closures(body)

        for m in CONNECT_RE.finditer(body):
            open_paren = m.end() - 1
            call_end = match_delim(body, open_paren, "(", ")")
            if call_end < 0:
                continue
            args = body[open_paren + 1 : call_end - 1]
            cm = re.search(r"(move\s+)?\|", args)
            if cm is None:
                continue
            sites += 1
            handler = expand_named_closures(
                closure_body(args, args.index("|", cm.start())), closures, body
            )

            target = m.group(1)
            pins = sorted(
                {
                    (ancestor, alias)
                    for ancestor in ancestors_of(edges, target)
                    for alias in clone_aliases(body, ancestor) - {ancestor}
                    if re.search(rf"\b{re.escape(alias)}\b", handler)
                }
            )
            if not pins:
                continue
            line = src[: fn_start + m.start()].count("\n") + 1
            key = (path, line, target, tuple(pins))
            if key in seen:
                continue
            seen.add(key)
            hits.append(
                (
                    "ancestor",
                    path,
                    line,
                    f"{target}.{m.group(2)}(..)  captures: "
                    + ", ".join(
                        f"{alias} (= {anc})" if alias != anc else alias
                        for anc, alias in pins
                    ),
                )
            )

    return sites


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
            line = src[: base + am.start()].count("\n") + 1

            # Rule (b), #1176: the closure holds a struct that *owns* the
            # target. Deliberately NOT gated on the discard below — a closure
            # can take its own `title` parameter and still capture the
            # `InfoWidgets` whose first field is that same label, and the pin
            # is just as total either way. There is no second-widget
            # carve-out to lose here: holding the owner is holding the target.
            held = sorted(
                a
                for a in owner_aliases(prefix, target)
                if re.search(rf"\b{re.escape(a)}\b", body)
            )
            if held:
                hits.append(
                    (
                        "field",
                        path,
                        line,
                        f"{m.group(1)}(.., {sigil}{target}, move |{param_name or '…'}, ..|)"
                        f"  captures the holder of `{target}`: " + ", ".join(held),
                    )
                )

            # Only `_`-prefixed first params are discards. A closure that uses
            # its own widget parameter is correct by construction, and a clone
            # alongside it is the second-widget carve-out.
            if not param_name.startswith("_"):
                continue

            used = sorted(
                a
                for a in clone_aliases(prefix, target)
                if a != target and re.search(rf"\b{re.escape(a)}\b", body)
            )
            if used:
                hits.append(
                    (
                        "bind",
                        path,
                        line,
                        f"{m.group(1)}(.., {sigil}{target}, move |{param_name}, ..|)"
                        "  captures: " + ", ".join(used),
                    )
                )

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
    # ── rule (a), #1176: a handler capturing one of its widget's ancestors ──
    (
        "ancestor pin (#1176 popover-menu shape, two hops up)",
        1,
        """
        fn build_device_menu(dev: &Device) -> gtk::MenuButton {
            let menu_btn = gtk::MenuButton::new();
            let popover = gtk::Popover::new();
            let pop_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
            let forget_btn = gtk::Button::with_label("Forget");
            let popover_for_forget = popover.clone();
            forget_btn.connect_clicked(move |_| {
                bluetooth::remove_device(&dev.path);
                popover_for_forget.popdown();
            });
            pop_box.append(&forget_btn);
            popover.set_child(Some(&pop_box));
            menu_btn.set_popover(Some(&popover));
            menu_btn
        }
        """,
    ),
    (
        "ancestor pin reached only through a named closure (#1176)",
        1,
        """
        fn build_create_popover(anchor: &gtk::MenuButton) -> gtk::Popover {
            let popover = gtk::Popover::new();
            let column = gtk::Box::new(gtk::Orientation::Vertical, 8);
            let create = gtk::Button::with_label("Add");
            let popover_for_create = popover.clone();
            let do_create = move || {
                tasks::create_task();
                popover_for_create.popdown();
            };
            let do_create_for_button = do_create.clone();
            create.connect_clicked(move |_| do_create_for_button());
            column.append(&create);
            popover.set_child(Some(&column));
            popover
        }
        """,
    ),
    (
        "sibling capture — a *different* widget, must stay clean (the carve-out)",
        0,
        """
        fn build_text_entry_row() -> gtk::Box {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let entry = gtk::Entry::new();
            let submit_btn = gtk::Button::with_label("Submit");
            let entry_for_submit = entry.clone();
            submit_btn.connect_clicked(move |_| submit_entry(&entry_for_submit));
            row.append(&entry);
            row.append(&submit_btn);
            row
        }
        """,
    ),
    (
        "child capture — a handler holding its own DESCENDANT is not a cycle",
        0,
        """
        fn build_popover() -> gtk::Popover {
            let popover = gtk::Popover::new();
            let entry = gtk::Entry::new();
            let entry_for_show = entry.clone();
            popover.connect_show(move |_| entry_for_show.grab_focus());
            popover.set_child(Some(&entry));
            popover
        }
        """,
    ),
    (
        "KNOWN GAP — the self case is not reported (flip to 1 with #1180)",
        0,
        """
        fn build_text_entry_row() -> gtk::Box {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            let entry = gtk::Entry::new();
            let entry_for_activate = entry.clone();
            entry.connect_activate(move |_| submit_entry(&entry_for_activate));
            row.append(&entry);
            row
        }
        """,
    ),
    # ── rule (b), #1176: a bind closure capturing the target's holder ──
    (
        "field pin (#1176 Media-page shape)",
        1,
        """
        fn wire_player_bind(w: &InfoWidgets, art_image: &gtk::Image) {
            let w = w.clone();
            let art = art_image.clone();
            let title = w.title.clone();
            bind(mpris::active_player(), &title, move |_, player| {
                render_player(&w, &art, &player);
            });
        }
        """,
    ),
    (
        "field pin is NOT excused by using the closure's own parameter",
        1,
        """
        fn wire_player_bind(w: &InfoWidgets) {
            let w = w.clone();
            let title = w.title.clone();
            bind(mpris::active_player(), &title, move |title, player| {
                title.set_text(&player.title);
                w.artist.set_text(&player.artists);
            });
        }
        """,
    ),
    (
        "a holder whose field is NOT the target must stay clean",
        0,
        """
        fn wire_seek(w: &InfoWidgets, seek: &gtk::Scale) {
            let w = w.clone();
            let pos = w.pos.clone();
            bind(mpris::position(), seek, move |seek, frac| {
                seek.set_value(frac);
                pos.set_text(&fmt(frac));
            });
        }
        """,
    ),
)


def scan_source(path: str, src: str, hits: list) -> tuple[int, int]:
    """Both scans over one source. Returns `(bind* sites, connect_* sites)`."""
    return scan_file(path, src, hits), scan_connect_pins(path, src, hits)


def self_test() -> list[str]:
    """Run the scanner over the fixtures; return a list of failure lines."""
    failures = []
    for name, expected, src in SELF_TEST_CASES:
        hits: list = []
        scan_source("<self-test>", src, hits)
        if len(hits) != expected:
            found = ", ".join(f"{kind}: {detail}" for kind, _, _, detail in hits) or "nothing"
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
    connect_sites = 0
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
                binds, connects = scan_source(path, src, hits)
                call_sites += binds
                connect_sites += connects

    # Flushed so the summary lands *before* the stderr report below when both
    # are funnelled into one build log.
    print(
        f"bind-pin scan: {files} files, {call_sites} bind* call sites, "
        f"{connect_sites} connect_* handlers, {len(hits)} pin(s)",
        flush=True,
    )

    for seen, floor, what, knobs in (
        (call_sites, MIN_CALL_SITES, "bind*", "BIND_FNS / MIN_CALL_SITES"),
        (connect_sites, MIN_CONNECT_SITES, "connect_*", "CONNECT_RE / MIN_CONNECT_SITES"),
    ):
        if seen < floor:
            print(
                f"\nERROR: only {seen} {what} call sites seen, expected at least {floor}.\n"
                f"The scan is not trustworthy — `{what}` was likely renamed or the roots\n"
                f"no longer hold the shell's widget code. Fix this script ({knobs} /\n"
                "DEFAULT_ROOTS) rather than lowering the floor to pass.",
                file=sys.stderr,
            )
            return 2

    if not hits:
        return 0

    print(f"\nERROR: {len(hits)} widget pin(s):\n", file=sys.stderr)
    for _kind, path, line, detail in hits:
        print(f"  {path}:{line}", file=sys.stderr)
        print(f"      {detail}", file=sys.stderr)

    kinds = {kind for kind, _, _, _ in hits}
    print(
        "\nEvery one of these keeps a widget alive past the point GTK would have freed\n"
        "it, defeating the WeakRef contract in\n"
        "crates/hytte-reactive/src/bind.rs:16-22 (#224, #772, #831, #1176).\n",
        file=sys.stderr,
    )
    if "bind" in kinds:
        print(
            "bind pin: the closure discards its own widget parameter and uses a strong\n"
            "clone of the same widget instead, so the binding owns the widget for its own\n"
            "lifetime — which for a service accessor is never.\n"
            "  Fix: drop the `let <name>_for_bind = <widget>.clone();` and use the\n"
            "  closure's parameter (`move |widget, value|`) in the body.\n",
            file=sys.stderr,
        )
    if "field" in kinds:
        print(
            "field pin: the closure captures a struct that *owns* bind's target (the\n"
            "target was built as `let <target> = <holder>.<field>.clone();`), so the\n"
            "binding holds the widget it is supposed to hold weakly.\n"
            "  Fix: move the field out of the holder and take it from the closure's own\n"
            "  parameter, or hand the closure weak handles.\n",
            file=sys.stderr,
        )
    if "ancestor" in kinds:
        print(
            "ancestor pin: a `connect_*` handler captures a strong clone of the widget it\n"
            "is attached to, or of one of that widget's ancestors in this very function\n"
            "(`popover → box → button`, and the handler lives on the button). The parent\n"
            "already owns the child, so the clone closes a refcount cycle GTK never\n"
            "breaks and the whole subtree outlives its container.\n"
            "  Fix: capture `<widget>.downgrade()` and `upgrade()` inside the handler, or\n"
            "  take the handler's own widget argument when it is the same widget.\n",
            file=sys.stderr,
        )
    print(
        "If the captured widget is genuinely a *different* widget — neither bind's\n"
        "target nor an ancestor of the handler's — this script should not have flagged\n"
        "it: see the carve-out section in nix/lint-bind-pins.py.",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
