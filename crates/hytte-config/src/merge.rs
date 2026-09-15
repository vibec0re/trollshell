//! The four layer-merge rules (#866/#868), stated once rather than
//! rediscovered per subsystem.
//!
//! A layered config gets subtly wrong in four specific places, so #866 settled
//! them as rules before any subsystem was migrated:
//!
//! | | rule |
//! |---|---|
//! | **scalars** | the overlay wins **when the key is present**, *unless a layer below locked that key* (#1227); an *absent* key falls through to the layer below, an explicitly-unset one removes it |
//! | **tables**  | deep merge, key by key |
//! | **arrays**  | **replace, never append** |
//! | **unknown keys** | warn, never fail |
//!
//! The first three live here, over `toml::Table`. The fourth is a property of
//! *deserialising* the merged table, so it lives in [`crate::subsystem`] where
//! the schema type is known.
//!
//! Rule 2 has one **reader-side corollary**, and it lives over there for the
//! same reason rule 4 does — answering it needs the schema. A table left
//! carrying no key the schema owns reads as **absent**, so an `Option<Table>`
//! over it is `None` rather than a struct full of defaults (#1025): since
//! #1008 the writer deliberately keeps a table the user has lines in — a
//! marker, a key the schema does not know — even when the schema's own value
//! went away, and a reader that turned those lines into defaults would write
//! the defaults back on the next save. Nothing here is involved in it: the
//! merged table is all it reads, and [`crate::subsystem::assemble`]'s own
//! `read_merged` argues it in full.
//!
//! # "Absent is not null", in a format with no null
//!
//! TOML has no null literal, so the difference between "I did not mention this
//! key" and "I want this key gone" has to be **spelled**. It is spelled
//! [`UNSET_KEY`]: an array of key names, honoured in the table it appears in.
//!
//! ```toml
//! # base layer, written by nix
//! [leds]
//! color = "amber"
//! brightness = 3
//! ```
//! ```toml
//! # your overlay
//! [leds]
//! brightness = 7        # present  -> wins
//! _unset = ["color"]    # explicit -> removed, so the code default applies
//! ```
//! …merges to `brightness = 7` and no `color` at all. Leave the `_unset` line
//! out and `color` stays `"amber"`: absence is inheritance, never erasure.
//!
//! The marker never survives into the merged table — a subsystem's schema
//! would otherwise have to know about it, and it would be reported as an
//! unknown key. "Never" is meant literally, at every depth and through every
//! shape: a marker in a table being *replaced* whole, and one inside an
//! array-of-tables element, are stripped on the way through too (#987). It is
//! only *honoured* inside a table; arrays replace whole, so there is nothing
//! in an array element for it to act on.
//!
//! # A marker the merge cannot honour
//!
//! `_unset = "color"` — a bare string rather than an array — is the obvious
//! typo, since every other value in these files is scalar. It removes nothing.
//! Dropping it in silence is the invisible-failure mode [`crate::subsystem`]
//! argues against for every other key shape in this crate (#988), so
//! [`malformed_unset`] finds every such marker and
//! [`crate::subsystem::assemble`] logs it **naming the layer file**. The
//! detection lives here, next to the code that honours the marker; the
//! reporting lives there, where the file name is — the same split, for the
//! same reason, as rule 4's unknown keys. Since #1018 `assemble` also returns
//! the finding as data, on [`crate::subsystem::Loaded::unset_findings`], the way
//! rule 4 returns unknown keys on `unknown_keys` — this module still never
//! sees a layer name, so the pairing happens where [`crate::subsystem`]
//! already computes one for the `warn!`.
//!
//! # A marker that names nothing
//!
//! `_unset = ["colr"]` is the *other* obvious typo, and the one that actually
//! costs the user their erasure: the shape is fine, so nothing above complains,
//! it removes nothing, and it is not an unknown key either — the marker is
//! stripped before the schema is ever shown the table, so rule 4 structurally
//! cannot see the name inside it (#1008). [`inert_unset`] finds it and
//! [`crate::subsystem::assemble`] warns about it, the same split of detection
//! from attribution as above.
//!
//! The test is **no layer sets this key at all**, deliberately, rather than the
//! narrower "the layers below this marker do not". Two ordinary patterns would
//! be noise under the narrower one: unsetting and re-setting a key in the same
//! overlay (documented above), and a marker in the bottom layer, where by
//! construction there is nothing below. What is left is a name that no file in
//! the search path — not the built-in default, not a base layer, not the
//! overlay itself — ever writes, which is a name that cannot be doing anything
//! for anyone.
//!
//! One false positive survives that, and it is the deliberate trade: a
//! portable overlay unsetting a key only *some* machines' base layers set, and
//! that [`crate::subsystem::Subsystem::DEFAULT_TOML`] does not document either.
//! It is a `warn!` that changes no behaviour, and the alternative is silence on
//! the likelier typo — the invisible failure this crate exists to argue
//! against.
//!
//! # A key a layer below has pinned (#1227)
//!
//! Rule 1 says "the overlay wins when present". Mara asked for the one
//! exception (#866, 2026-09-13: "i want the values i set in nix to not be
//! shadowable in the state one"), Annika settled it on by default and per key
//! (#866, 2026-09-15: "fine by me that nix config has precedence - as long as
//! nix configuration options can be optional"), and this is where it is
//! honoured.
//!
//! A layer declares the keys it pins in [`LOCKED_KEY`], beside the values
//! themselves — nix renders one entry per option leaf the operator actually
//! set, so an option nobody sets locks nothing and the "options stay optional"
//! half is a property of the *rendering* rather than a knob here:
//!
//! ```toml
//! # base layer, written by nix
//! _locked = ["brightness", "core.color"]
//! brightness = 3
//!
//! [core]
//! color = "amber"
//! ```
//! ```toml
//! # your overlay
//! brightness = 7           # refused, reported: the base layer pinned it
//! [core]
//! color = "cyan"           # refused, reported
//! label = "front"          # fine: an unlocked sibling still overlays
//! ```
//!
//! Three properties are worth stating, because each is a way to get this
//! wrong:
//!
//! * **A lock binds the layers *above* it, never the one it is written in.**
//!   [`merge_all_locked`] accumulates the set as it folds, so the locking
//!   layer's own values land first and every later layer is measured against
//!   them. That is also why the overlay's own [`LOCKED_KEY`] is inert rather
//!   than special-cased away: there is no layer above it to bind.
//! * **A refusal is reported, never silent.** [`Merged::shadowed`] names the
//!   layer and the key for every attempt, once per key per layer, and
//!   [`crate::subsystem::assemble`] turns each into the journal line and the
//!   [`crate::subsystem::Finding`] a settings UI reads. Dropping the value
//!   without saying so is the invisible failure the three sections above
//!   already argue against; this one costs the operator an edit they believe
//!   took effect.
//! * **[`UNSET_KEY`] cannot bypass it.** `_unset = ["brightness"]` against a
//!   locked key is an override attempt like any other — it is refused and
//!   reported, or the lock would be one line away from meaningless.
//!
//! Paths are dotted from the table the marker appears in, so a top-level
//! `_locked = ["core.color"]` and a `_locked = ["color"]` inside `[core]` mean
//! the same thing. Nix renders the first spelling. Locking a *table* path
//! locks it whole; locking a leaf under it leaves its siblings alone, and a
//! replacement that would take a locked descendant with it (a scalar written
//! over a table holding one) is refused as a whole. Arrays are atomic, because
//! rule 3 makes them atomic: locking an array key locks the array.
//!
//! The marker never reaches the schema, for [`UNSET_KEY`]'s reasons and by the
//! same mechanism; [`malformed_locked`] is [`malformed_unset`]'s twin, so a
//! `_locked = "brightness"` that pins nothing is said out loud rather than
//! dropped.

use std::collections::BTreeSet;
use std::fmt;

/// Reserved key naming the keys to drop from the layer below.
///
/// Underscore-prefixed because TOML bare keys allow it and no subsystem schema
/// in this workspace uses that shape, so it cannot collide with a real key.
pub const UNSET_KEY: &str = "_unset";

/// Reserved key naming the keys **this layer pins against every layer above
/// it** (#1227) — see the module docs.
///
/// Underscore-prefixed for [`UNSET_KEY`]'s reason, and deliberately the same
/// spelling shape: the two markers are the only vocabulary this module adds to
/// a subsystem's file, and a reader who has met one should recognise the other.
pub const LOCKED_KEY: &str = "_locked";

/// One override a [`LOCKED_KEY`] refused, as data.
///
/// Carries no message, unlike [`MalformedUnset`] and [`InertUnset`]: the
/// sentence the operator needs names the subsystem *and* the file they must
/// edit ("`core-leds.brightness` is set in nix and cannot be overridden from
/// ~/.config/trollshell/core-leds.toml"), and this module has never seen
/// either. [`crate::subsystem::assemble`] composes it, from the one format
/// string there, so there is no second rendering here to drift from it.
///
/// `#[non_exhaustive]` for [`InertUnset`]'s reason: every construction site is
/// in this crate and a third field (the layer that declared the lock, say) is
/// plausible.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Shadowed {
    /// Index into the layers handed to [`merge_all_locked`] — the layer that
    /// tried to override, not the one that locked the key.
    pub layer: usize,
    /// Dotted path of the key the override was refused on.
    pub key: String,
}

/// What [`merge_all_locked`] learned folding the layers together.
///
/// `#[non_exhaustive]`: every construction site is in this crate, and this is
/// the shape a fifth merge rule would grow a field on.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct Merged {
    /// The merged table, with every [`UNSET_KEY`] and [`LOCKED_KEY`] marker
    /// gone — the same table [`merge_all`] returns.
    pub table: toml::Table,
    /// Dotted paths every layer declared locked, unioned. Returned as data so
    /// a settings UI can grey a row (and a save can skip it) without
    /// re-parsing the base layers itself.
    pub locked: BTreeSet<String>,
    /// Every override the locks refused, in layer order. One entry per key per
    /// layer: an overlay that both unsets and re-sets the same locked key made
    /// one mistake and gets one report.
    pub shadowed: Vec<Shadowed>,
}

/// Where a lock refusal is recorded while one layer is merged.
///
/// Threaded as an `Option` rather than being folded into a second copy of
/// [`merge_into`]: the two functions would be the same forty lines with four
/// `if`s moved, and "the locked path is skipped" has to hold at every depth
/// and in the [`UNSET_KEY`] arm as well as the assignment one.
struct Locks<'a> {
    locked: &'a BTreeSet<String>,
    layer: usize,
    shadowed: &'a mut Vec<Shadowed>,
    /// Paths already reported **for this layer**, so one key costs one report
    /// however many ways this layer tried to move it.
    seen: &'a mut BTreeSet<String>,
}

impl Locks<'_> {
    /// Whether a *whole-value* write at `path` may not happen, recording the
    /// refusal if so.
    ///
    /// Refused when `path` is locked itself **or** when it is the root of a
    /// subtree holding a locked path: rules 1 and 3 replace whole, so writing
    /// a scalar over a table would take a locked leaf underneath it with it,
    /// and the refusal has to be whole too.
    fn refuses(&mut self, path: &str) -> bool {
        let under = format!("{path}.");
        let hit = self.locked.contains(path) || self.locked.iter().any(|l| l.starts_with(&under));
        hit && self.record(path)
    }

    /// [`Self::refuses`] for the deep-merge arm, where a lock *inside* the
    /// table is honoured by the recursion rather than here: only an exact lock
    /// on the table itself stops the descent.
    fn refuses_exactly(&mut self, path: &str) -> bool {
        self.locked.contains(path) && self.record(path)
    }

    /// Always `true` — the return is a convenience for the two predicates
    /// above, not a verdict.
    fn record(&mut self, path: &str) -> bool {
        if self.seen.insert(path.to_owned()) {
            self.shadowed.push(Shadowed {
                layer: self.layer,
                key: path.to_owned(),
            });
        }
        true
    }
}

/// Merge `overlay` onto `base` in place, applying the three structural rules.
///
/// Only keys the overlay actually mentions are touched, which is the whole of
/// "absent falls through": there is no branch that removes an unmentioned key,
/// because there is no code that looks at one.
///
/// A malformed [`UNSET_KEY`] is dropped rather than honoured; run
/// [`malformed_unset`] over the same table *before* merging if you want to
/// **say** so.
///
/// [`LOCKED_KEY`] markers are stripped but **not honoured** here: a lock binds
/// the layers *above* the one that declared it, and a two-table merge has no
/// layer order to read that from. [`merge_all_locked`] is where locking
/// happens.
pub fn merge_into(base: &mut toml::Table, overlay: &toml::Table) {
    merge_into_locked(base, overlay, "", &mut None);
}

fn merge_into_locked(
    base: &mut toml::Table,
    overlay: &toml::Table,
    prefix: &str,
    locks: &mut Option<Locks<'_>>,
) {
    // The explicit-unset half of "absent is not null". Runs first so an
    // overlay may both unset an inherited key and set a fresh value for it.
    if let Some(names) = overlay.get(UNSET_KEY).and_then(toml::Value::as_array) {
        for name in names.iter().filter_map(toml::Value::as_str) {
            // #1227: erasing a locked key is an override like any other. Left
            // out, the lock would be one `_unset` line away from meaningless —
            // the removed key falls back to `DEFAULT_TOML`, which is exactly
            // the value nix was asked to displace.
            if locks
                .as_mut()
                .is_some_and(|l| l.refuses(&format!("{prefix}{name}")))
            {
                continue;
            }
            base.remove(name);
        }
    }

    for (key, value) in overlay {
        if key == UNSET_KEY || key == LOCKED_KEY {
            continue;
        }
        let path = format!("{prefix}{key}");
        // Tables: deep merge, key by key. Without this branch the whole
        // sub-table would be replaced and every key the overlay did not
        // restate would vanish.
        //
        // A union of keys, so the result carries a key the schema owns iff
        // some layer did — which is what lets rule 2's reader-side corollary
        // (#1025, module docs) be asked once of the merged table rather than
        // layer by layer, and lets a marker's erasure be honoured *before* it
        // is asked. The corollary reads nothing but the merged table: whether
        // a block ended up empty because a marker emptied it or because the
        // user typed it that way is a difference it deliberately does not
        // consult (#1088 review, M2).
        //
        // An `if let`/`else` rather than the two-armed `match` this was until
        // #1227: the lock check made each branch a block, which is exactly
        // when `clippy::single_match_else` starts asking for this shape.
        if let (Some(toml::Value::Table(into)), toml::Value::Table(from)) =
            (base.get_mut(key.as_str()), value)
        {
            // Only an *exact* lock stops the descent. A lock deeper inside is
            // honoured one level down, which is what lets a locked
            // `core.color` sit beside an overlayable `core.label` in the table
            // the overlay is restating.
            if locks.as_mut().is_some_and(|l| l.refuses_exactly(&path)) {
                continue;
            }
            merge_into_locked(into, from, &format!("{path}."), locks);
        } else {
            // Everything else — scalars, arrays, and a table with nothing (or
            // a scalar) under it — replaces whole. An array is deliberately
            // not concatenated or merged element-wise: appending leaves no way
            // to remove an inherited element.
            //
            // The lock refusal here is the *whole-value* one, subtree included
            // (#1227): a scalar written over a table carrying a locked leaf
            // would take that leaf with it, and there is no partial
            // replacement for this merge to fall back on.
            //
            // The replacement goes through `stripped` rather than being cloned
            // verbatim, which is what makes "the marker never survives" true
            // inside it too — including inside an array-of-tables element,
            // where a verbatim clone used to carry the marker through to the
            // schema as a bogus unknown key (#987).
            if locks.as_mut().is_some_and(|l| l.refuses(&path)) {
                continue;
            }
            base.insert(key.clone(), stripped(value));
        }
    }
}

/// `value` with every [`UNSET_KEY`] marker removed, at every depth and through
/// arrays as well as tables.
///
/// This is the *replacement* path, so no marker inside it has anything to act
/// on: whatever stood under this key in the layer below is gone whole (rules 1
/// and 3) before the replacement lands. Stripping is therefore the complete
/// treatment, not half of one.
fn stripped(value: &toml::Value) -> toml::Value {
    match value {
        // Merging into an empty table applies the marker to nothing (correct:
        // there is nothing below it) and strips it, recursing on the way.
        toml::Value::Table(from) => {
            let mut fresh = toml::Table::new();
            merge_into(&mut fresh, from);
            toml::Value::Table(fresh)
        }
        toml::Value::Array(items) => toml::Value::Array(items.iter().map(stripped).collect()),
        other => other.clone(),
    }
}

/// A reserved marker whose shape the merge cannot honour — a [`UNSET_KEY`]
/// (from [`malformed_unset`]) or, since #1227, a [`LOCKED_KEY`] (from
/// [`malformed_locked`]).
///
/// The name predates the second marker and is kept because the type is public,
/// `#[non_exhaustive]` and identical for both: [`Self::key`] names the
/// offending marker, whichever it is, and [`Self::found`] the type found where
/// an array of key names was expected. The caller knows which function it
/// called; [`crate::subsystem::FindingKind`] is where the two are told apart
/// for a reader who did not.
///
/// Reported rather than logged: the merge has never seen a file name, and with
/// three or four candidate layers in play an unattributed "your marker is
/// malformed" is close to useless.
///
/// `#[non_exhaustive]` because a third field is plausible — the layer it came
/// from, if [`crate::subsystem::Loaded`] ever returns these the way it returns
/// unknown keys (#1008) — and it costs nothing here: every construction site
/// is inside this crate.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct MalformedUnset {
    /// Dotted path of the offending marker, or of the offending element
    /// inside it: `_unset`, `core._unset`, `core._unset[1]`.
    pub key: String,
    /// The TOML type actually found where an array of key names was expected.
    pub found: &'static str,
}

impl fmt::Display for MalformedUnset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} should be a key name, but is a {}",
            self.key, self.found
        )
    }
}

/// Every [`UNSET_KEY`] marker in `table` that [`merge_into`] will drop without
/// honouring: one that is not an array, and every non-string element of one
/// that is.
///
/// Walks nested tables, and *only* tables — a marker inside an array element
/// is stripped unconditionally and means nothing there (arrays replace whole,
/// so there is nothing under it to unset; #987), which makes its shape not a
/// thing to complain about. Run this on each layer **before** merging: after
/// the merge every marker is gone, well-formed or not.
///
/// [`MalformedUnset::key`] is a dotted path, so a quoted TOML key containing a
/// literal `.` reads as a nesting separator here — the same ambiguity
/// [`crate::subsystem::collect_paths`] documents. It is diagnostic-only in
/// this direction: the worst case is a warning that points at a slightly wrong
/// path, never a key acted on that should not have been.
#[must_use]
pub fn malformed_unset(table: &toml::Table) -> Vec<MalformedUnset> {
    let mut out = Vec::new();
    collect_malformed(table, UNSET_KEY, "", &mut out);
    out
}

/// [`malformed_unset`]'s twin for [`LOCKED_KEY`] (#1227): every marker
/// [`merge_all_locked`] will read past without pinning anything.
///
/// The same walk with the same reporting split, because it is the same defect
/// one marker over — and a worse one to leave silent. A dropped `_unset` costs
/// the user an erasure they can see did not happen; a dropped `_locked` in a
/// nix-written base layer costs them the guarantee the whole feature is
/// (#866: "the values I set in nix are not shadowable"), silently, in a file
/// they never open.
#[must_use]
pub fn malformed_locked(table: &toml::Table) -> Vec<MalformedUnset> {
    let mut out = Vec::new();
    collect_malformed(table, LOCKED_KEY, "", &mut out);
    out
}

fn collect_malformed(
    table: &toml::Table,
    marker: &'static str,
    prefix: &str,
    out: &mut Vec<MalformedUnset>,
) {
    match table.get(marker) {
        None => {}
        Some(toml::Value::Array(names)) => {
            for (i, name) in names.iter().enumerate() {
                if name.as_str().is_none() {
                    out.push(MalformedUnset {
                        key: format!("{prefix}{marker}[{i}]"),
                        found: name.type_str(),
                    });
                }
            }
        }
        Some(other) => out.push(MalformedUnset {
            key: format!("{prefix}{marker}"),
            found: other.type_str(),
        }),
    }

    for (key, value) in table {
        // Already reported above, and never a table worth descending into
        // even when somebody writes one there. Both markers are skipped
        // whichever one is being checked: descending into the *other* one
        // would attribute its contents a path that does not exist.
        if key == UNSET_KEY || key == LOCKED_KEY {
            continue;
        }
        if let toml::Value::Table(nested) = value {
            collect_malformed(nested, marker, &format!("{prefix}{key}."), out);
        }
    }
}

/// A well-formed [`UNSET_KEY`] name that matches no key in any layer, so it
/// erases nothing anywhere.
///
/// `#[non_exhaustive]` for the same reason [`MalformedUnset`] is: every
/// construction site is in this crate, and a third field is plausible.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct InertUnset {
    /// Index into the `layers` slice handed to [`inert_unset`] — the layer the
    /// marker was written in. This module has never seen a file name; the
    /// caller that has turns this back into one.
    pub layer: usize,
    /// Dotted path of the key the marker names, not of the marker itself:
    /// `colr`, `core.colr`. It is the thing the user has to go and correct.
    pub key: String,
}

impl fmt::Display for InertUnset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} is set by no layer, so unsetting it does nothing",
            self.key
        )
    }
}

/// Every well-formed [`UNSET_KEY`] name across `layers` that names a key none
/// of them sets — see the module docs for why the test spans *all* the layers
/// rather than the ones below each marker.
///
/// Malformed markers are [`malformed_unset`]'s business and are skipped here: a
/// marker that is not an array of names has no names to check, and a non-string
/// element is not a name. Reporting both about the same marker would be two
/// complaints for one typo.
///
/// Only tables are walked, and only table keys count as "set" — the same
/// restriction, for the same reason, as [`malformed_unset`]: arrays replace
/// whole, so a marker means nothing inside one and nothing inside one is
/// addressable by a marker outside it.
///
/// [`InertUnset::key`] is a dotted path, with the quoted-key ambiguity
/// [`malformed_unset`] documents; it is diagnostic-only in this direction too.
#[must_use]
pub fn inert_unset(layers: &[toml::Table]) -> Vec<InertUnset> {
    let mut present = BTreeSet::new();
    for layer in layers {
        collect_key_paths(layer, "", &mut present);
    }

    let mut out = Vec::new();
    for (layer, table) in layers.iter().enumerate() {
        collect_inert(table, "", &present, layer, &mut out);
    }
    out
}

/// Every dotted key path in `table`, tables included, marker keys excluded —
/// "what this layer sets". A marker ([`UNSET_KEY`] or [`LOCKED_KEY`]) is
/// machinery, never a key a sibling marker could be naming, so
/// `_unset = ["_locked"]` is inert rather than a key that "exists".
fn collect_key_paths(table: &toml::Table, prefix: &str, out: &mut BTreeSet<String>) {
    for (key, value) in table {
        if key == UNSET_KEY || key == LOCKED_KEY {
            continue;
        }
        let path = format!("{prefix}{key}");
        if let toml::Value::Table(nested) = value {
            collect_key_paths(nested, &format!("{path}."), out);
        }
        out.insert(path);
    }
}

fn collect_inert(
    table: &toml::Table,
    prefix: &str,
    present: &BTreeSet<String>,
    layer: usize,
    out: &mut Vec<InertUnset>,
) {
    if let Some(names) = table.get(UNSET_KEY).and_then(toml::Value::as_array) {
        // Deduped **within one marker**, and only there: `_unset = ["aa", "aa"]`
        // is one mistake on one line and gets one warning. The same name in two
        // different layers is two files to go and edit, so those stay separate.
        let mut said = BTreeSet::new();
        for name in names.iter().filter_map(toml::Value::as_str) {
            let key = format!("{prefix}{name}");
            if !present.contains(&key) && said.insert(key.clone()) {
                out.push(InertUnset { layer, key });
            }
        }
    }

    for (key, value) in table {
        if key == UNSET_KEY || key == LOCKED_KEY {
            continue;
        }
        if let toml::Value::Table(nested) = value {
            collect_inert(nested, &format!("{prefix}{key}."), present, layer, out);
        }
    }
}

/// [`merge_into`], taking `base` by value.
#[must_use]
pub fn merge(mut base: toml::Table, overlay: &toml::Table) -> toml::Table {
    merge_into(&mut base, overlay);
    base
}

/// Fold every layer together, **lowest precedence first** — the order
/// [`crate::xdg::Env::config_layers`] hands them back in.
///
/// Starts from an empty table rather than from the first layer so that even
/// the bottom layer is normalised: an [`UNSET_KEY`] down there refers to
/// nothing and is simply dropped, instead of leaking into the result.
///
/// [`merge_all_locked`]'s table, for the callers that have no use for the
/// other two halves. There is deliberately no "merge without honouring the
/// locks" spelling: one fold, one rule.
#[must_use]
pub fn merge_all<I: IntoIterator<Item = toml::Table>>(layers: I) -> toml::Table {
    merge_all_locked(layers).table
}

/// [`merge_all`], returning the lock set and every override it refused
/// alongside the merged table (#1227).
///
/// The fold is what makes "a lock binds the layers above it" true: each
/// layer is merged against the locks declared *so far*, and only then does its
/// own [`LOCKED_KEY`] join the set. The locking layer's own values therefore
/// land normally, and the top layer — the operator's overlay — can declare
/// locks all it likes and bind nothing, which is the right answer rather than
/// a case to special-case.
///
/// A malformed [`LOCKED_KEY`] pins nothing; run [`malformed_locked`] over each
/// layer *before* folding if you want to **say** so, the same way
/// [`malformed_unset`] pairs with [`merge_into`].
#[must_use]
pub fn merge_all_locked<I: IntoIterator<Item = toml::Table>>(layers: I) -> Merged {
    let mut out = Merged::default();
    for (layer, table) in layers.into_iter().enumerate() {
        // Scoped so the shared borrow of `out.locked` ends before the line
        // below extends it — the fold's whole ordering rule, in two lines.
        {
            let mut seen = BTreeSet::new();
            let mut locks = Some(Locks {
                locked: &out.locked,
                layer,
                shadowed: &mut out.shadowed,
                seen: &mut seen,
            });
            merge_into_locked(&mut out.table, &table, "", &mut locks);
        }
        collect_locks(&table, "", &mut out.locked);
    }
    out
}

/// Every path `table` declares locked, dotted from `table`'s own root.
///
/// Walks nested tables, so a marker inside `[core]` locks `core.<name>` — the
/// same relative-to-its-table reading [`UNSET_KEY`] has. Nix renders the
/// top-level dotted spelling; a hand-written base layer may use either, and
/// neither is silently swallowed. Arrays are not walked, for
/// [`malformed_unset`]'s reason: rule 3 replaces them whole, so nothing inside
/// one is addressable by a marker outside it.
///
/// A name that is itself dotted is taken as a path, which is what makes the
/// two spellings equivalent — and carries the quoted-TOML-key ambiguity
/// [`MalformedUnset::key`] documents, in the direction that at worst locks a
/// key nobody asked to lock in a schema no member of this workspace has.
fn collect_locks(table: &toml::Table, prefix: &str, out: &mut BTreeSet<String>) {
    if let Some(names) = table.get(LOCKED_KEY).and_then(toml::Value::as_array) {
        for name in names.iter().filter_map(toml::Value::as_str) {
            out.insert(format!("{prefix}{name}"));
        }
    }

    for (key, value) in table {
        if key == UNSET_KEY || key == LOCKED_KEY {
            continue;
        }
        if let toml::Value::Table(nested) = value {
            collect_locks(nested, &format!("{prefix}{key}."), out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(text: &str) -> toml::Table {
        text.parse().expect("fixture parses")
    }

    fn merged(base: &str, overlay: &str) -> toml::Table {
        merge(table(base), &table(overlay))
    }

    /// **Rule 1, both halves.** A key the overlay states wins; a key it does
    /// not state is inherited, not erased.
    ///
    /// Red if the `base.insert` in the fallthrough arm goes away (`brightness`
    /// stays 3), and red if the merge is ever replaced by "overlay wins whole"
    /// (`color` disappears).
    #[test]
    fn a_scalar_the_overlay_states_wins_and_one_it_omits_falls_through() {
        let out = merged(
            r#"
            color = "amber"
            brightness = 3
            "#,
            "brightness = 7",
        );

        assert_eq!(out["brightness"].as_integer(), Some(7), "present -> wins");
        assert_eq!(
            out["color"].as_str(),
            Some("amber"),
            "absent is inheritance, not erasure"
        );
    }

    /// **Rule 1, the explicit-unset half.** The spelled-out "null".
    ///
    /// Red if the [`UNSET_KEY`] block in [`merge_into`] is deleted (`color`
    /// survives), and red if the `continue` is deleted (`_unset` leaks into
    /// the merged table, where the schema would then report it as an unknown
    /// key).
    #[test]
    fn an_explicit_unset_removes_an_inherited_key_and_leaves_no_marker() {
        let out = merged(
            r#"
            color = "amber"
            brightness = 3
            "#,
            r#"_unset = ["color"]"#,
        );

        assert!(!out.contains_key("color"), "explicitly unset -> removed");
        assert_eq!(
            out["brightness"].as_integer(),
            Some(3),
            "unsetting one key must not disturb its neighbours"
        );
        assert!(
            !out.contains_key(UNSET_KEY),
            "the marker is merge machinery and must not reach the schema"
        );
    }

    /// Unsetting and re-setting the same key in one overlay: the value wins,
    /// because the removal is applied before the assignments.
    #[test]
    fn unset_then_set_in_the_same_layer_keeps_the_new_value() {
        let out = merged(
            r#"color = "amber""#,
            r#"
            _unset = ["color"]
            color = "cyan"
            "#,
        );

        assert_eq!(out["color"].as_str(), Some("cyan"));
    }

    /// A nested `_unset` is honoured at its own level, and never survives —
    /// including when the base has no counterpart table for it to act on, and
    /// including inside an array-of-tables element, which replaces whole and
    /// used to be cloned verbatim, marker and all (#987).
    ///
    /// Red if `stripped` stops recursing through arrays (`entry[0]._unset`
    /// survives, and `assemble` then reports it as an unknown key), and red
    /// if it stops recursing through the tables inside them
    /// (`entry[0].deep._unset` does).
    #[test]
    fn unset_works_at_every_table_depth_and_never_survives() {
        let out = merged(
            r#"
            [leds.core]
            color = 1
            width = 2

            [[entry]]
            name = "old"
            "#,
            r#"
            [leds.core]
            _unset = ["color"]

            [fresh.branch]
            _unset = ["nothing-here"]
            kept = true

            [[entry]]
            _unset = ["name"]
            name = "A"

            [entry.deep]
            _unset = ["also-nothing"]
            nested = true
            "#,
        );

        let core = out["leds"]["core"].as_table().expect("table survives");
        assert!(!core.contains_key("color"));
        assert_eq!(core["width"].as_integer(), Some(2));
        assert!(!core.contains_key(UNSET_KEY));

        let fresh = out["fresh"]["branch"].as_table().expect("branch created");
        assert_eq!(fresh["kept"].as_bool(), Some(true));
        assert!(
            !fresh.contains_key(UNSET_KEY),
            "a table with no base counterpart must still be stripped"
        );

        let entries = out["entry"].as_array().expect("array of tables");
        assert_eq!(entries.len(), 1, "arrays still replace whole");
        let entry = entries[0].as_table().expect("element is a table");
        assert_eq!(
            entry["name"].as_str(),
            Some("A"),
            "a marker inside an array element acts on nothing, not on its own siblings"
        );
        assert!(
            !entry.contains_key(UNSET_KEY),
            "a marker inside an array element must not reach the schema either"
        );
        let deep = entry["deep"].as_table().expect("nested table survives");
        assert_eq!(deep["nested"].as_bool(), Some(true));
        assert!(
            !deep.contains_key(UNSET_KEY),
            "…nor one in a table nested inside an array element"
        );
    }

    /// **Rule 2.** Deep merge, key by key, at more than one level — one level
    /// deep would pass even if the recursion only ever ran once.
    ///
    /// Red the moment the table arm in [`merge_into`] stops recursing: `x`
    /// and `sibling` both vanish under a wholesale replace.
    #[test]
    fn tables_deep_merge_key_by_key() {
        let out = merged(
            r"
            [a.b]
            x = 1
            y = 2

            [a.sibling]
            kept = true
            ",
            r"
            [a.b]
            y = 3
            z = 4
            ",
        );

        let b = out["a"]["b"].as_table().expect("a.b is a table");
        assert_eq!(b["x"].as_integer(), Some(1), "untouched key survives");
        assert_eq!(b["y"].as_integer(), Some(3), "restated key is overridden");
        assert_eq!(b["z"].as_integer(), Some(4), "new key is added");
        assert_eq!(
            out["a"]["sibling"]["kept"].as_bool(),
            Some(true),
            "an untouched sibling table survives"
        );
    }

    /// **Rule 3.** Replace, never append — and never element-wise either.
    ///
    /// Red if an append arm is ever added (the result would be four entries),
    /// and red if arrays were zipped positionally (the result would keep
    /// `"S85"`/`"S9"` in the tail).
    #[test]
    fn arrays_replace_and_are_never_appended_to() {
        let out = merged(r#"lines = ["S8", "S85", "S9"]"#, r#"lines = ["S1"]"#);

        let lines: Vec<&str> = out["lines"]
            .as_array()
            .expect("array")
            .iter()
            .filter_map(toml::Value::as_str)
            .collect();
        assert_eq!(
            lines,
            ["S1"],
            "an overlay array is the whole answer, so an inherited element can be removed"
        );
    }

    /// The same rule for an array *of tables*, which is the shape a naive
    /// deep-merge is most tempted to recurse into.
    #[test]
    fn arrays_of_tables_replace_rather_than_merging_element_wise() {
        let out = merged(
            r#"
            [[place]]
            name = "Office"
            station = "900110001"

            [[place]]
            name = "Cabin"
            "#,
            r#"
            [[place]]
            name = "Studio"
            "#,
        );

        let places = out["place"].as_array().expect("array of tables");
        assert_eq!(places.len(), 1, "two entries must not survive one");
        assert_eq!(places[0]["name"].as_str(), Some("Studio"));
        assert!(
            places[0].get("station").is_none(),
            "no key may bleed through from the replaced element"
        );
    }

    /// A key whose *type* changes between layers is a replace, in both
    /// directions — there is nothing sensible to deep-merge across kinds.
    #[test]
    fn a_type_change_replaces_in_either_direction() {
        let to_table = merged("x = 1", "[x]\na = 2");
        assert_eq!(to_table["x"]["a"].as_integer(), Some(2));

        let to_scalar = merged("[x]\na = 2", "x = 1");
        assert_eq!(to_scalar["x"].as_integer(), Some(1));
    }

    /// [`merge_all`] applies layers left to right, later winning — the order
    /// [`crate::xdg::Env::config_layers`] produces.
    ///
    /// Three layers, not two: with two, a fold that applied them in reverse
    /// would be indistinguishable from one that dropped the middle.
    #[test]
    fn merge_all_applies_layers_left_to_right() {
        let out = merge_all([
            table("a = 1\nb = 1\nc = 1"),
            table("b = 2\nc = 2"),
            table("c = 3"),
        ]);

        assert_eq!(
            out["a"].as_integer(),
            Some(1),
            "only the bottom layer set a"
        );
        assert_eq!(
            out["b"].as_integer(),
            Some(2),
            "the middle layer set b last"
        );
        assert_eq!(
            out["c"].as_integer(),
            Some(3),
            "the top layer wins outright"
        );
    }

    /// An `_unset` in the bottom layer refers to nothing, and must not reach
    /// the schema as a stray key.
    #[test]
    fn merge_all_strips_an_unset_marker_from_the_bottom_layer() {
        let out = merge_all([table("_unset = [\"nothing\"]\na = 1")]);

        assert_eq!(out["a"].as_integer(), Some(1));
        assert!(!out.contains_key(UNSET_KEY));
    }

    #[test]
    fn merge_all_of_nothing_is_empty() {
        assert!(merge_all(Vec::new()).is_empty());
    }

    // ── #988: a marker the merge cannot honour ──────────────────────────────

    /// A bare string is the obvious typo. It is found, with its type named, so
    /// the caller that *does* know the file name can say so.
    ///
    /// Red if the non-array arm of `collect_malformed` goes away.
    #[test]
    fn a_marker_that_is_not_an_array_is_reported_with_its_type() {
        assert_eq!(
            malformed_unset(&table(r#"_unset = "color""#)),
            [MalformedUnset {
                key: "_unset".into(),
                found: "string",
            }]
        );
        assert_eq!(
            malformed_unset(&table("[core]\n_unset = 3\n")),
            [MalformedUnset {
                key: "core._unset".into(),
                found: "integer",
            }],
            "and at depth, with the path that leads to it"
        );
    }

    /// A well-formed array with one element that is not a key name: the array
    /// is still honoured for its string elements, and only the offender is
    /// named — by index, since a key name is exactly what it lacks.
    ///
    /// Red if the element loop in `collect_malformed` goes away.
    #[test]
    fn a_non_string_element_is_reported_by_index_and_its_neighbours_still_apply() {
        let overlay = table("[core]\n_unset = [\"color\", 3]\n");

        assert_eq!(
            malformed_unset(&overlay),
            [MalformedUnset {
                key: "core._unset[1]".into(),
                found: "integer",
            }]
        );

        let out = merge(table("[core]\ncolor = \"amber\"\nwidth = 2\n"), &overlay);
        let core = out["core"].as_table().expect("table");
        assert!(
            !core.contains_key("color"),
            "the usable element is honoured"
        );
        assert_eq!(core["width"].as_integer(), Some(2));
    }

    /// The guarantee that must not be traded away for the warning: a malformed
    /// marker is still stripped, so it never reaches the schema as an unknown
    /// key on top of being useless.
    #[test]
    fn a_malformed_marker_is_still_stripped_at_every_depth() {
        let out = merged(
            r#"color = "amber""#,
            "_unset = 3\n\n[core]\n_unset = \"width\"\n",
        );

        assert!(!out.contains_key(UNSET_KEY));
        assert!(
            !out["core"]
                .as_table()
                .expect("table")
                .contains_key(UNSET_KEY)
        );
        assert_eq!(
            out["color"].as_str(),
            Some("amber"),
            "and it removes nothing, which is the whole reason it must be said out loud"
        );
    }

    /// A well-formed marker is not a complaint — the warning has to be a
    /// signal, not noise on every file that uses the feature.
    #[test]
    fn a_well_formed_marker_is_not_reported() {
        assert!(
            malformed_unset(&table(
                "_unset = [\"a\"]\n\n[core]\n_unset = [\"b\", \"c\"]\n"
            ))
            .is_empty()
        );
    }

    /// A marker inside an array element is stripped without comment: arrays
    /// replace whole, so there is nothing under it to unset and its shape
    /// cannot matter (#987). Only tables are walked.
    #[test]
    fn a_marker_inside_an_array_element_is_not_reported() {
        assert!(malformed_unset(&table("[[entry]]\n_unset = 3\nname = \"A\"\n")).is_empty());
    }

    // ── #1008: a marker that names nothing ──────────────────────────────────

    /// The typo that costs the erasure: well-formed, so #988 says nothing, and
    /// stripped before the schema sees it, so rule 4 cannot say anything
    /// either. Named by the path of the *key*, at depth, with the layer it was
    /// written in — the caller turns that index back into a file name.
    ///
    /// Red if the `present.contains` test in `collect_inert` goes away
    /// (`core.color` is reported too), and red if the recursion into nested
    /// tables goes away (nothing is reported at all).
    #[test]
    fn a_marker_naming_a_key_no_layer_sets_is_reported_with_its_layer() {
        let layers = [
            table("[core]\ncolor = \"amber\"\n"),
            table("[core]\n_unset = [\"colr\", \"color\"]\n"),
        ];

        assert_eq!(
            inert_unset(&layers),
            [InertUnset {
                layer: 1,
                key: "core.colr".into(),
            }],
            "only the name nothing sets; `color` is set one layer down"
        );
    }

    /// The noise guard, and the reason the test spans every layer rather than
    /// only the ones below the marker: a key set *above* the marker, or in the
    /// marker's own table, is a documented pattern and must stay silent.
    ///
    /// Red if `collect_key_paths` is narrowed to the layers below each marker,
    /// or if it stops walking nested tables.
    #[test]
    fn a_marker_is_silent_when_any_layer_sets_the_key_it_names() {
        // Set in the same table as the marker (the "unset then set" pattern).
        assert!(
            inert_unset(&[table("_unset = [\"color\"]\ncolor = \"cyan\"\n")]).is_empty(),
            "unset-then-set in one layer is documented, not a typo"
        );

        // Set only in a layer the merge applies *after* this one.
        assert!(
            inert_unset(&[
                table("[core]\n_unset = [\"color\"]\n"),
                table("[core]\ncolor = \"cyan\"\n"),
            ])
            .is_empty(),
            "a key some other layer sets is a real key, wherever it sits"
        );

        // A whole table, which is a key path like any other (#987's N4 shape).
        assert!(
            inert_unset(&[table("[core]\nx = 1\n"), table("_unset = [\"core\"]\n")]).is_empty(),
            "a marker may name a table, and a table is set"
        );
    }

    /// One typo, one complaint. A marker #988 already reports has no names to
    /// check (not an array) or a name that is not one (a non-string element),
    /// so it must not draw a second, differently-worded warning here.
    ///
    /// Red if `collect_inert` stops filtering on `as_array`/`as_str`.
    #[test]
    fn a_malformed_marker_is_not_also_reported_as_naming_nothing() {
        assert!(
            inert_unset(&[table("_unset = \"colr\"\n")]).is_empty(),
            "not an array: #988's business, and it has no names in it"
        );
        assert!(
            inert_unset(&[table("_unset = [3]\n")]).is_empty(),
            "not a name: #988's business too"
        );
    }

    /// One line, one mistake, one complaint — signal-to-noise is the whole
    /// justification for this check, so a name written twice in the same marker
    /// must not warn twice. Across *layers* it still does: two files each carry
    /// a line to go and fix.
    ///
    /// Red if the per-marker dedupe goes away (found by #1016's review).
    #[test]
    fn a_name_repeated_inside_one_marker_is_reported_once() {
        assert_eq!(
            inert_unset(&[table("_unset = [\"aa\", \"aa\", \"bb\"]\n")])
                .iter()
                .map(|i| i.key.as_str())
                .collect::<Vec<_>>(),
            ["aa", "bb"]
        );

        assert_eq!(
            inert_unset(&[table("_unset = [\"aa\"]\n"), table("_unset = [\"aa\"]\n")])
                .iter()
                .map(|i| (i.layer, i.key.as_str()))
                .collect::<Vec<_>>(),
            [(0, "aa"), (1, "aa")],
            "two layers is two files, so two lines in the journal"
        );
    }

    /// The sentence a caller with no field-structured log can print — the
    /// sibling [`InertUnset`]'s sentence is pinned the same way, immediately
    /// below.
    ///
    /// Red if `MalformedUnset`'s `Display` is ever reworded: every other
    /// assertion on this type compares two `MalformedUnset`s (or two
    /// `Finding`s built from the same `to_string()` call) to each other, so
    /// nothing else in the suite notices the *text* changing (#1018 review
    /// M-2 — measured: rewording the format string left the whole suite
    /// green, while the mirror mutation on `InertUnset` reddened the test
    /// below).
    #[test]
    fn a_malformed_unset_says_what_is_wrong_in_one_line() {
        assert_eq!(
            MalformedUnset {
                key: "core._unset".into(),
                found: "string",
            }
            .to_string(),
            "core._unset should be a key name, but is a string"
        );
    }

    /// The sentence a caller with no field-structured log can print, matching
    /// [`MalformedUnset`]'s, pinned immediately above.
    #[test]
    fn an_inert_unset_says_what_is_wrong_in_one_line() {
        assert_eq!(
            InertUnset {
                layer: 1,
                key: "core.colr".into(),
            }
            .to_string(),
            "core.colr is set by no layer, so unsetting it does nothing"
        );
    }

    /// Every offender is reported, not just the first, and one marker's typo
    /// does not mask its neighbour's.
    #[test]
    fn every_name_that_matches_nothing_is_reported() {
        let out = inert_unset(&[table(
            "_unset = [\"aa\", \"bb\"]\n\n[core]\n_unset = [\"cc\"]\nkept = 1\n",
        )]);

        let keys: Vec<&str> = out.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(keys, ["aa", "bb", "core.cc"]);
    }

    // ── #1227: the per-key nix lock ─────────────────────────────────────────

    /// Fold `bodies` the way [`crate::subsystem::assemble`] does — lowest
    /// precedence first — and hand back everything the locks learned.
    fn folded(bodies: &[&str]) -> Merged {
        merge_all_locked(bodies.iter().map(|b| table(b)))
    }

    /// The headline rule. A base layer that pins a key keeps its value
    /// whatever the overlay says, and the attempt is reported rather than
    /// swallowed.
    ///
    /// Red if the `refuses` guard in the fallthrough arm of `merge_into_locked`
    /// goes away (`brightness` becomes 7), and red if the `shadowed.push` goes
    /// away (no report).
    #[test]
    fn a_locked_scalar_keeps_the_base_value_and_the_attempt_is_reported() {
        let out = folded(&[
            "brightness = 1\n",
            "_locked = [\"brightness\"]\nbrightness = 3\n",
            "brightness = 7\n",
        ]);

        assert_eq!(
            out.table["brightness"].as_integer(),
            Some(3),
            "nix has precedence per key (#866, 2026-09-15)"
        );
        assert_eq!(
            out.shadowed,
            vec![Shadowed {
                layer: 2,
                key: "brightness".into(),
            }],
            "and the refusal is named, once"
        );
        assert_eq!(
            out.locked,
            BTreeSet::from(["brightness".to_owned()]),
            "the set the caller greys rows from"
        );
    }

    /// Per **key**, not per file or per table: a locked key inside a table the
    /// overlay is also editing costs that key and nothing else.
    ///
    /// Red if the deep-merge arm stops recursing with the lock context, and
    /// red if `refuses_exactly` in that arm is widened to `refuses` (the whole
    /// `[core]` table would be refused and `label` would be lost).
    #[test]
    fn a_locked_nested_key_is_kept_while_an_unlocked_sibling_still_overlays() {
        let out = folded(&[
            "_locked = [\"core.color\"]\n\n[core]\ncolor = \"amber\"\nlabel = \"front\"\n",
            "[core]\ncolor = \"cyan\"\nlabel = \"back\"\n",
        ]);

        let core = out.table["core"].as_table().expect("a table");
        assert_eq!(core["color"].as_str(), Some("amber"), "locked -> kept");
        assert_eq!(
            core["label"].as_str(),
            Some("back"),
            "its unlocked sibling still follows rule 1"
        );
        let keys: Vec<&str> = out.shadowed.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(keys, ["core.color"], "one key refused, one report");
    }

    /// Rule 3 makes arrays atomic, so a locked array is atomic: the overlay's
    /// whole replacement is refused rather than merged element-wise.
    #[test]
    fn a_locked_array_is_refused_whole() {
        let out = folded(&[
            "_locked = [\"palette\"]\npalette = [\"amber\", \"rust\"]\n",
            "palette = [\"cyan\"]\n",
        ]);

        let palette: Vec<&str> = out.table["palette"]
            .as_array()
            .expect("an array")
            .iter()
            .filter_map(toml::Value::as_str)
            .collect();
        assert_eq!(palette, ["amber", "rust"]);
        assert_eq!(out.shadowed.len(), 1, "{:?}", out.shadowed);
    }

    /// [`UNSET_KEY`] is an override attempt like any other. Without this the
    /// lock is one line away from meaningless: the key would fall through to
    /// `DEFAULT_TOML`, which is exactly the value nix was asked to displace.
    ///
    /// Red if the `refuses` guard in the [`UNSET_KEY`] loop goes away.
    #[test]
    fn an_unset_cannot_erase_a_locked_key() {
        let out = folded(&[
            "_locked = [\"brightness\"]\nbrightness = 3\n",
            "_unset = [\"brightness\"]\n",
        ]);

        assert_eq!(
            out.table["brightness"].as_integer(),
            Some(3),
            "erasing a locked key is overriding it"
        );
        assert_eq!(out.shadowed.len(), 1, "{:?}", out.shadowed);
    }

    /// One key, one report — however many ways one layer tried to move it.
    ///
    /// Red if the `seen` dedup in `Locks::record` goes away (two reports for
    /// the one mistake), which is the mutation the "reported once at load"
    /// half of the design is about.
    #[test]
    fn an_overlay_that_both_unsets_and_sets_a_locked_key_is_reported_once() {
        let out = folded(&[
            "_locked = [\"brightness\"]\nbrightness = 3\n",
            "_unset = [\"brightness\"]\nbrightness = 7\n",
        ]);

        assert_eq!(out.table["brightness"].as_integer(), Some(3));
        assert_eq!(
            out.shadowed,
            vec![Shadowed {
                layer: 1,
                key: "brightness".into(),
            }],
            "one mistake, one line to read"
        );
    }

    /// A lock binds the layers **above** it and never the one that wrote it:
    /// the locking layer's own value lands normally, or a base layer could not
    /// set the key it is pinning.
    ///
    /// Red if [`merge_all_locked`] extends the lock set before merging the
    /// layer rather than after (the base's own `brightness = 3` would be
    /// refused and the result would be the bottom layer's 1).
    #[test]
    fn a_lock_binds_the_layers_above_it_and_not_the_one_that_wrote_it() {
        let out = folded(&[
            "brightness = 1\n",
            "_locked = [\"brightness\"]\nbrightness = 3\n",
        ]);

        assert_eq!(out.table["brightness"].as_integer(), Some(3));
        assert!(out.shadowed.is_empty(), "{:?}", out.shadowed);
    }

    /// …which is also why the top layer's own marker is inert rather than
    /// special-cased away: there is nothing above it to bind.
    #[test]
    fn an_overlays_own_lock_binds_nothing() {
        let out = folded(&[
            "brightness = 3\n",
            "_locked = [\"brightness\"]\nbrightness = 7\n",
        ]);

        assert_eq!(out.table["brightness"].as_integer(), Some(7));
        assert!(out.shadowed.is_empty(), "{:?}", out.shadowed);
    }

    /// Two `XDG_CONFIG_DIRS` entries both declaring locks union, and the
    /// earlier one still binds the later one.
    #[test]
    fn locks_from_two_base_layers_union() {
        let out = folded(&[
            "_locked = [\"a\"]\na = 1\nb = 1\nc = 1\n",
            "_locked = [\"b\"]\na = 2\nb = 2\nc = 2\n",
            "a = 9\nb = 9\nc = 9\n",
        ]);

        assert_eq!(
            out.locked,
            BTreeSet::from(["a".to_owned(), "b".to_owned()]),
            "the union, not the last one's"
        );
        assert_eq!(out.table["a"].as_integer(), Some(1), "locked by layer 0");
        assert_eq!(out.table["b"].as_integer(), Some(2), "locked by layer 1");
        assert_eq!(out.table["c"].as_integer(), Some(9), "unlocked -> rule 1");

        let named: Vec<(usize, &str)> = out
            .shadowed
            .iter()
            .map(|s| (s.layer, s.key.as_str()))
            .collect();
        assert_eq!(
            named,
            [(1, "a"), (2, "a"), (2, "b")],
            "layer 1's own attempt on `a` is refused too — a lock binds every \
             layer above it, not only the top one"
        );
    }

    /// A scalar written over a table holding a locked leaf is refused
    /// **whole**: rules 1 and 3 replace whole, so a partial replacement is not
    /// a thing this merge can express, and letting it through would take the
    /// locked leaf with it.
    ///
    /// Red if `Locks::refuses`' subtree test (the `starts_with` half) goes
    /// away.
    #[test]
    fn a_scalar_written_over_a_table_holding_a_locked_leaf_is_refused_whole() {
        let out = folded(&[
            "_locked = [\"core.color\"]\n\n[core]\ncolor = \"amber\"\n",
            "core = \"red\"\n",
        ]);

        let core = out.table["core"]
            .as_table()
            .expect("the table survives the attempted replacement");
        assert_eq!(core["color"].as_str(), Some("amber"));
        let keys: Vec<&str> = out.shadowed.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(keys, ["core"]);
    }

    /// Locking a table locks it whole — the one lock that is not a leaf, and
    /// the reason [`crate::subsystem::Loaded::is_locked`] asks about ancestors.
    #[test]
    fn locking_a_table_locks_every_key_under_it() {
        let out = folded(&[
            "_locked = [\"core\"]\n\n[core]\ncolor = \"amber\"\n",
            "[core]\ncolor = \"cyan\"\nlabel = \"back\"\n",
        ]);

        let core = out.table["core"].as_table().expect("a table");
        assert_eq!(core["color"].as_str(), Some("amber"));
        assert!(
            !core.contains_key("label"),
            "a locked table takes its whole shape with it: {core:?}"
        );
        let keys: Vec<&str> = out.shadowed.iter().map(|s| s.key.as_str()).collect();
        assert_eq!(keys, ["core"]);
    }

    /// The two spellings mean the same thing, so a hand-written base layer can
    /// use either and nix's dotted one is not privileged.
    #[test]
    fn a_nested_locked_marker_means_the_same_as_a_dotted_top_level_one() {
        let nested = folded(&[
            "[core]\n_locked = [\"color\"]\ncolor = \"amber\"\n",
            "[core]\ncolor = \"cyan\"\n",
        ]);
        let dotted = folded(&[
            "_locked = [\"core.color\"]\n\n[core]\ncolor = \"amber\"\n",
            "[core]\ncolor = \"cyan\"\n",
        ]);

        assert_eq!(nested.locked, BTreeSet::from(["core.color".to_owned()]));
        assert_eq!(nested.locked, dotted.locked);
        assert_eq!(nested.table, dotted.table);
    }

    /// The marker is machinery and must never reach the schema — at every
    /// depth, and through a replacement, exactly as [`UNSET_KEY`] must not.
    ///
    /// Red if the `key == LOCKED_KEY` skip in `merge_into_locked` goes away.
    #[test]
    fn the_locked_marker_never_reaches_the_merged_table() {
        let out = folded(&[
            "_locked = [\"a\"]\na = 1\n\n[core]\n_locked = [\"color\"]\ncolor = \"amber\"\n",
            "core = { _locked = [\"nope\"], fresh = 2 }\n",
        ]);

        assert!(!out.table.contains_key(LOCKED_KEY), "{:?}", out.table);
        let core = out.table["core"].as_table().expect("a table");
        assert!(
            !core.contains_key(LOCKED_KEY),
            "not at depth either: {core:?}"
        );
    }

    /// [`merge_all`] is [`merge_all_locked`]'s table, so there is no second
    /// merge with a second rule — a caller that wants only the table still
    /// gets the locks honoured.
    #[test]
    fn merge_all_honours_a_lock_too() {
        let out = merge_all([
            table("_locked = [\"brightness\"]\nbrightness = 3\n"),
            table("brightness = 7\n"),
        ]);

        assert_eq!(out["brightness"].as_integer(), Some(3));
    }

    /// A `_locked` the fold cannot read pins nothing, so it is found rather
    /// than dropped in silence — the worse half of [`malformed_unset`]'s
    /// argument, since the layer that wrote it is nix's.
    #[test]
    fn a_malformed_locked_marker_is_found_and_pins_nothing() {
        let bad = table("_locked = \"brightness\"\nbrightness = 3\n");

        let found = malformed_locked(&bad);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].key, "_locked");
        assert_eq!(found[0].found, "string");
        assert_eq!(
            found[0].to_string(),
            "_locked should be a key name, but is a string"
        );

        let out = merge_all_locked([bad, table("brightness = 7\n")]);
        assert!(out.locked.is_empty(), "{:?}", out.locked);
        assert_eq!(
            out.table["brightness"].as_integer(),
            Some(7),
            "it really pinned nothing — which is what the finding is for"
        );
        assert!(!out.table.contains_key(LOCKED_KEY), "{:?}", out.table);
    }

    /// Each marker's check sees only its own shape: a well-formed `_unset`
    /// beside a malformed `_locked` produces one finding, not two.
    #[test]
    fn the_two_marker_checks_do_not_report_each_other() {
        let mixed = table("_unset = [\"gone\"]\n_locked = 3\ngone = 1\n");

        assert!(malformed_unset(&mixed).is_empty(), "{mixed:?}");
        let locked = malformed_locked(&mixed);
        assert_eq!(locked.len(), 1, "{locked:?}");
        assert_eq!(locked[0].key, "_locked");
        assert_eq!(locked[0].found, "integer");
    }
}
