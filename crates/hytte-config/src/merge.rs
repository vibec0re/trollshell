//! The four layer-merge rules (#866/#868), stated once rather than
//! rediscovered per subsystem.
//!
//! A layered config gets subtly wrong in four specific places, so #866 settled
//! them as rules before any subsystem was migrated:
//!
//! | | rule |
//! |---|---|
//! | **scalars** | the overlay wins **when the key is present**; an *absent* key falls through to the layer below, an explicitly-unset one removes it |
//! | **tables**  | deep merge, key by key |
//! | **arrays**  | **replace, never append** |
//! | **unknown keys** | warn, never fail |
//!
//! The first three live here, over `toml::Table`. The fourth is a property of
//! *deserialising* the merged table, so it lives in [`crate::subsystem`] where
//! the schema type is known.
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
//! same reason, as rule 4's unknown keys.

use std::fmt;

/// Reserved key naming the keys to drop from the layer below.
///
/// Underscore-prefixed because TOML bare keys allow it and no subsystem schema
/// in this workspace uses that shape, so it cannot collide with a real key.
pub const UNSET_KEY: &str = "_unset";

/// Merge `overlay` onto `base` in place, applying the three structural rules.
///
/// Only keys the overlay actually mentions are touched, which is the whole of
/// "absent falls through": there is no branch that removes an unmentioned key,
/// because there is no code that looks at one.
///
/// A malformed [`UNSET_KEY`] is dropped rather than honoured; run
/// [`malformed_unset`] over the same table *before* merging if you want to
/// **say** so.
pub fn merge_into(base: &mut toml::Table, overlay: &toml::Table) {
    // The explicit-unset half of "absent is not null". Runs first so an
    // overlay may both unset an inherited key and set a fresh value for it.
    if let Some(names) = overlay.get(UNSET_KEY).and_then(toml::Value::as_array) {
        for name in names.iter().filter_map(toml::Value::as_str) {
            base.remove(name);
        }
    }

    for (key, value) in overlay {
        if key == UNSET_KEY {
            continue;
        }
        match (base.get_mut(key.as_str()), value) {
            // Tables: deep merge, key by key. Without this arm the whole
            // sub-table would be replaced and every key the overlay did not
            // restate would vanish.
            (Some(toml::Value::Table(into)), toml::Value::Table(from)) => merge_into(into, from),
            // Everything else — scalars, arrays, and a table with nothing (or
            // a scalar) under it — replaces whole. An array is deliberately
            // not concatenated or merged element-wise: appending leaves no way
            // to remove an inherited element.
            //
            // The replacement goes through `stripped` rather than being cloned
            // verbatim, which is what makes "the marker never survives" true
            // inside it too — including inside an array-of-tables element,
            // where a verbatim clone used to carry the marker through to the
            // schema as a bogus unknown key (#987).
            _ => {
                base.insert(key.clone(), stripped(value));
            }
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

/// A [`UNSET_KEY`] marker whose shape [`merge_into`] cannot honour.
///
/// Reported rather than logged: the merge has never seen a file name, and with
/// three or four candidate layers in play an unattributed "your `_unset` is
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
    collect_malformed(table, "", &mut out);
    out
}

fn collect_malformed(table: &toml::Table, prefix: &str, out: &mut Vec<MalformedUnset>) {
    match table.get(UNSET_KEY) {
        None => {}
        Some(toml::Value::Array(names)) => {
            for (i, name) in names.iter().enumerate() {
                if name.as_str().is_none() {
                    out.push(MalformedUnset {
                        key: format!("{prefix}{UNSET_KEY}[{i}]"),
                        found: name.type_str(),
                    });
                }
            }
        }
        Some(other) => out.push(MalformedUnset {
            key: format!("{prefix}{UNSET_KEY}"),
            found: other.type_str(),
        }),
    }

    for (key, value) in table {
        // Already reported above, and never a table worth descending into
        // even when somebody writes one there.
        if key == UNSET_KEY {
            continue;
        }
        if let toml::Value::Table(nested) = value {
            collect_malformed(nested, &format!("{prefix}{key}."), out);
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
#[must_use]
pub fn merge_all<I: IntoIterator<Item = toml::Table>>(layers: I) -> toml::Table {
    let mut out = toml::Table::new();
    for layer in layers {
        merge_into(&mut out, &layer);
    }
    out
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
}
