//! What a config family's leaves **are**, declared once and checked against
//! the documented default (#888 P0, spec §2a/§2b).
//!
//! [`crate::subsystem`] already knows how a family is *read* — the search
//! path, the four merge rules, the lock, the format-preserving writer. What it
//! has never known is what the individual keys are: the reader's only model of
//! a family is `S`'s serde shape, which says `toml::Value` for every key it
//! cares about typing tolerantly (#1040 T1) and therefore says nothing at all.
//! A settings UI that wants to render *one row per leaf* — a switch for a
//! boolean, a spin button for a bounded integer, a combo for a fixed
//! vocabulary — cannot get any of that from the serde type, and asking each
//! family to hand-write a form is how the four families end up disagreeing
//! with their own `DEFAULT_TOML`.
//!
//! So a family declares a [`Schema`] beside its
//! [`DEFAULT_TOML`](crate::subsystem::Subsystem::DEFAULT_TOML) — one [`Field`]
//! per leaf, each with the dotted path the lock set already speaks
//! ([`crate::subsystem::Loaded::is_locked`]), a [`Kind`] that says what the
//! value may be, and one sentence of `doc`. The long explanation stays in
//! `DEFAULT_TOML`'s comments, which is where a person reads it.
//!
//! # The walker is what makes the declaration trustworthy
//!
//! A hand-written schema beside a hand-written default is two hand-written
//! things that drift, which is the defect `checks.config-vocab` exists for one
//! level up (`nix/lint-config-vocab.py`). [`verify`] is the same idea in-tree
//! and per family: it parses `DEFAULT_TOML` and fails on
//!
//! 1. a leaf the default states that no [`Field`] claims ([`Mismatch::Forgot`]),
//! 2. a [`Field`] whose path the default does not state
//!    ([`Mismatch::Invented`]),
//! 3. a default value outside its own [`Kind`] ([`Mismatch::BadDefault`]) — a
//!    bool where `Int` was declared, an integer outside `min..=max`, a string
//!    outside `options`.
//!
//! Every family calls it from one `#[test]` next to its `impl Subsystem`, so
//! adding a key to a `DEFAULT_TOML` without adding its `Field` is a red test
//! rather than a row the form silently cannot draw.
//!
//! # Two things the walker deliberately does not do
//!
//! **A collection's absence is not [`Mismatch::Invented`].** [`Kind::Map`] and
//! [`Kind::List`] describe keys whose *contents* are the operator's, and the
//! only way a `DEFAULT_TOML` could state "there are none" is an empty literal —
//! a value the format-preserving writer would then have to round-trip.
//! `workspaces.toml`'s default is comments only for exactly that reason, and
//! `agents.toml` leaves its whole `[display.<name>]` block commented out. Both
//! would otherwise have every one of their fields reported as invented. The
//! cost is that a *typo* in a collection field's path is not caught here; §6
//! P3 (the vocabulary lint reading these schemas) is what closes that, because
//! it compares against the nix option tree rather than against the default.
//!
//! **It reads a file, not a Rust type.** `verify` says the schema and the
//! documented default agree; it cannot say either agrees with `S`'s serde
//! fields. Each family already pins *that* half with its own
//! `the_shipped_default_parses_and_matches_the_rust_default` test, which is
//! why the three tests together are a closed loop and none of them is
//! redundant.

use std::collections::{BTreeMap, BTreeSet};

use crate::merge;

/// One config family's leaves.
///
/// Not `#[non_exhaustive]`, unlike most of this crate's types: every [`Schema`]
/// in the tree is a `const` struct literal written *outside* this crate (in
/// `hytte-config-families`, in `hytte-plugin-stats`, in `hytte-plugin-agents`),
/// so sealing the literal would seal the feature.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Schema {
    /// The family's name — [`crate::subsystem::Subsystem::NAME`], i.e. the
    /// config file's stem (`core-leds`, not `core_leds`).
    pub family: &'static str,
    /// One entry per leaf, in the order a form should render them — which is
    /// the order `DEFAULT_TOML` documents them in, so the page and the file
    /// read the same way down.
    pub fields: &'static [Field],
}

impl Schema {
    /// The [`Field`] at a dotted `path`, or `None`.
    ///
    /// Top-level fields only: a [`Kind::Map`]'s sub-fields are relative to an
    /// entry the schema does not name, so they have no dotted path from the
    /// root to look up by.
    #[must_use]
    pub fn field(&self, path: &str) -> Option<&Field> {
        self.fields.iter().find(|field| field.path == path)
    }
}

/// One leaf of a [`Schema`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Field {
    /// The dotted TOML path, exactly as the lock set spells it
    /// ([`crate::subsystem::Loaded::is_locked`]) — one spelling serves the
    /// schema, the lock and the nix vocabulary.
    ///
    /// **Inside a [`Kind::Map`] this is relative to one entry**, and is a
    /// single segment: `display.<name>.label` has no fixed spelling, so the
    /// map's own field carries `display` and its sub-field carries `label`.
    pub path: &'static str,
    /// What the value may be — and, for a form, which row to draw.
    pub kind: Kind,
    /// One sentence. The paragraph stays in `DEFAULT_TOML`'s comments above
    /// the key, which is where a person reads it and what a form shows as the
    /// row's tooltip.
    pub doc: &'static str,
}

/// What a [`Field`]'s value may be.
///
/// The vocabulary is deliberately the *form's*, not TOML's: each variant is a
/// row a settings page can draw and a rule [`verify`] can check, which is why
/// there is a [`Kind::Color`] rather than a second `Text` and why
/// [`Kind::Int`] carries its bounds instead of leaving them to the parser.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `true` / `false` — a switch row.
    Bool,
    /// A whole number in `min..=max`, both ends inclusive — a spin row.
    ///
    /// The bounds are the *parser's* bounds restated so a form can refuse a
    /// value before writing it; `verify` checks the documented default against
    /// them, and each family's own test checks them against its parser's
    /// constants.
    Int {
        /// Lowest accepted value, inclusive.
        min: i64,
        /// Highest accepted value, inclusive.
        max: i64,
    },
    /// One of a fixed vocabulary — a combo row.
    Choice {
        /// The accepted spellings, in the order a combo should offer them.
        options: &'static [&'static str],
    },
    /// Free text — an entry row.
    Text {
        /// Whether the empty string (or one that is all whitespace) is a
        /// legal value. `false` for `agents`' `socket`, which must be an
        /// absolute path.
        blank_ok: bool,
    },
    /// A colour: one of `options`, or an `#rrggbb` / bare `rrggbb` literal —
    /// an entry row with a swatch.
    ///
    /// The one place §2a's sketch needed a field it did not have. `core-leds`'
    /// `color` accepts *both* a named map and a literal, and a single
    /// [`Field`] carries a single [`Kind`], so a unit variant would have had
    /// nowhere to keep `heat` / `style` / `rainbow` / `transpride` — leaving
    /// `verify` unable to tell the family's own default (`"heat"`) from a
    /// typo. §2a's own comment spells the intent, *"a Choice **OR**
    /// `#rrggbb`"*; this is that sentence with somewhere to put the Choice.
    Color {
        /// The named colours, beside the literal spelling.
        options: &'static [&'static str],
    },
    /// An array whose elements are all of one kind — read-only chips in v1
    /// (spec §1; `List` editing is P2).
    ///
    /// A [`Kind::Map`] **as the element kind** describes the element table
    /// itself: the array index is the key level a map otherwise spends on a
    /// name. That is `workspaces`' `apps = [{ id = "…", exec = "…" }]`.
    List(&'static Kind),
    /// A table whose keys the schema does not know — `display.<name>`,
    /// `workspace.<name>` — each entry described by these fields. Read-only in
    /// v1 (spec §1; `Map` editing is P2).
    Map(&'static [Field]),
}

impl Kind {
    /// Whether this kind's *contents* are the operator's rather than the
    /// schema's — a [`Kind::List`] or a [`Kind::Map`].
    ///
    /// What exempts a field from [`Mismatch::Invented`]; see the module docs
    /// for why a collection may legitimately be absent from a documented
    /// default and a scalar may not.
    #[must_use]
    pub const fn is_collection(&self) -> bool {
        matches!(self, Self::List(_) | Self::Map(_))
    }

    /// Whether `value` fits this kind.
    ///
    /// The same walk [`verify`] runs over a documented default, asked as a
    /// yes/no — so a form's "refuse this before saving it" and CI's "the
    /// default is inside its own kind" can never answer differently.
    #[must_use]
    pub fn accepts(&self, value: &toml_edit::Value) -> bool {
        let mut out = Vec::new();
        check_item("", self, &toml_edit::Item::Value(value.clone()), &mut out);
        out.is_empty()
    }

    /// What this kind accepts, phrased to read after *"expected"* — the same
    /// shape [`crate::subsystem::env::EnvKnob`]'s `file_accepts` is phrased
    /// in, so a schema-driven rejection and a parser-driven one sound alike.
    #[must_use]
    pub fn expected(&self) -> String {
        match self {
            Self::Bool => "true or false".to_owned(),
            Self::Int { min, max } => format!("a whole number, {min}..={max}"),
            Self::Choice { options } => format!("one of {}", options.join(", ")),
            Self::Text { blank_ok: true } => "any text".to_owned(),
            Self::Text { blank_ok: false } => "text that is not blank".to_owned(),
            Self::Color { options } => {
                format!("one of {}, or an #rrggbb literal", options.join(", "))
            }
            Self::List(elem) => format!("an array of ({})", elem.expected()),
            Self::Map(_) => "a table of named entries".to_owned(),
        }
    }
}

/// One way a [`Schema`] and its family's `DEFAULT_TOML` disagree.
///
/// `#[non_exhaustive]` — every one is built by [`verify`] inside this crate,
/// and the three classes §2b names have already grown a fourth
/// ([`Self::Unparsable`]).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Mismatch {
    /// The default states a leaf no [`Field`] claims: the schema forgot it, so
    /// a form would silently not draw a key the operator can see in the file.
    Forgot {
        /// Dotted path of the leaf.
        path: String,
    },
    /// A [`Field`] whose path the default does not state, and which is not a
    /// collection: the schema invented it, so a form would draw a row whose
    /// documented default does not exist.
    Invented {
        /// The field's declared path.
        path: String,
    },
    /// The default's own value at this path is outside the [`Kind`] declared
    /// for it — the one class that says the *rule* is wrong rather than that
    /// a key is missing from one side.
    BadDefault {
        /// Dotted path of the leaf.
        path: String,
        /// The value as the file spells it.
        found: String,
        /// [`Kind::expected`].
        expected: String,
    },
    /// `DEFAULT_TOML` is not valid TOML at all. Not one of §2b's three
    /// classes — it means the bug is ours rather than a disagreement between
    /// two of our own declarations — but [`verify`] must report it rather than
    /// panic, and a family whose default does not parse has bigger problems
    /// than its schema.
    Unparsable {
        /// The parser's message.
        message: String,
    },
}

impl Mismatch {
    /// The dotted path this mismatch is about, or `""` for
    /// [`Self::Unparsable`], which is about the whole file.
    #[must_use]
    pub fn path(&self) -> &str {
        match self {
            Self::Forgot { path } | Self::Invented { path } | Self::BadDefault { path, .. } => path,
            Self::Unparsable { .. } => "",
        }
    }
}

impl std::fmt::Display for Mismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Forgot { path } => write!(
                f,
                "{path} is stated in the documented default but no Field claims it"
            ),
            Self::Invented { path } => write!(
                f,
                "{path} is declared as a Field but the documented default does not state it"
            ),
            Self::BadDefault {
                path,
                found,
                expected,
            } => write!(
                f,
                "{path} defaults to {found}, which is outside its declared kind; expected {expected}"
            ),
            Self::Unparsable { message } => {
                write!(f, "the documented default is not valid TOML: {message}")
            }
        }
    }
}

/// Check `schema` against the `DEFAULT_TOML` it describes.
///
/// The module docs carry the three classes and the two deliberate blind spots.
/// Every family calls this from one `#[test]`.
///
/// # Errors
/// One [`Mismatch`] per disagreement, in path order within each class — never
/// only the first, so a schema written against a stale default reds once and
/// lists everything.
pub fn verify(schema: &Schema, default_toml: &str) -> Result<(), Vec<Mismatch>> {
    let doc = match default_toml.parse::<toml_edit::DocumentMut>() {
        Ok(doc) => doc,
        Err(e) => {
            return Err(vec![Mismatch::Unparsable {
                message: e.to_string(),
            }]);
        }
    };

    let index: BTreeMap<&str, &Field> = schema
        .fields
        .iter()
        .map(|field| (field.path, field))
        .collect();

    let mut claimed = BTreeSet::new();
    let mut out = Vec::new();
    walk(doc.as_table(), "", &index, &mut claimed, &mut out);

    // Declared but not documented. After the walk, so the two lists read in
    // the order a reviewer would check them: what the file has that the schema
    // does not, then what the schema has that the file does not.
    for field in schema.fields {
        if !claimed.contains(field.path) && !field.kind.is_collection() {
            out.push(Mismatch::Invented {
                path: field.path.to_owned(),
            });
        }
    }

    if out.is_empty() { Ok(()) } else { Err(out) }
}

/// One table level of [`verify`]'s walk over the documented default.
///
/// A key the schema claims is checked against its [`Kind`] and **not**
/// descended into — that is what makes a [`Kind::Map`]'s entries the map's
/// business rather than a pile of [`Mismatch::Forgot`]s. A key it does not
/// claim is either structure (a non-empty table, `[bar]` existing so that
/// `bar.cpu` can) or a leaf nobody declared.
fn walk(
    table: &dyn toml_edit::TableLike,
    prefix: &str,
    index: &BTreeMap<&str, &Field>,
    claimed: &mut BTreeSet<String>,
    out: &mut Vec<Mismatch>,
) {
    for (key, item) in table.iter() {
        if key == merge::UNSET_KEY || key == merge::LOCKED_KEY {
            continue;
        }
        let path = format!("{prefix}{key}");
        if let Some(field) = index.get(path.as_str()) {
            claimed.insert(path.clone());
            check_item(&path, &field.kind, item, out);
            continue;
        }
        match item.as_table_like() {
            Some(sub) if !sub.is_empty() => walk(sub, &format!("{path}."), index, claimed, out),
            _ => out.push(Mismatch::Forgot { path }),
        }
    }
}

/// `item` against `kind`, appending one [`Mismatch`] per disagreement.
///
/// The shared core of [`verify`] and [`Kind::accepts`], over
/// [`toml_edit::Item`] rather than [`toml_edit::Value`] because a standard
/// `[display.argus]` table is an `Item` and never a `Value`.
fn check_item(path: &str, kind: &Kind, item: &toml_edit::Item, out: &mut Vec<Mismatch>) {
    match kind {
        Kind::Map(fields) => {
            let Some(table) = item.as_table_like() else {
                out.push(bad(path, item, kind));
                return;
            };
            for (name, entry) in table.iter() {
                if name == merge::UNSET_KEY || name == merge::LOCKED_KEY {
                    continue;
                }
                check_record(&format!("{path}.{name}"), fields, entry, out);
            }
        }
        Kind::List(elem) => {
            let Some(array) = item.as_array() else {
                out.push(bad(path, item, kind));
                return;
            };
            for (index, value) in array.iter().enumerate() {
                let at = format!("{path}[{index}]");
                let entry = toml_edit::Item::Value(value.clone());
                match elem {
                    // An array of records: the index is the key level a map
                    // otherwise spends on a name (see `Kind::List`).
                    Kind::Map(fields) => check_record(&at, fields, &entry, out),
                    other => check_item(&at, other, &entry, out),
                }
            }
        }
        scalar => {
            let Some(value) = item.as_value() else {
                out.push(bad(path, item, kind));
                return;
            };
            if !fits(scalar, value) {
                out.push(bad(path, item, kind));
            }
        }
    }
}

/// One entry of a [`Kind::Map`] — a table whose keys are the map's sub-fields.
///
/// A key no sub-field claims is [`Mismatch::Forgot`], exactly as at the top
/// level. A sub-field the entry does not carry is **not**
/// [`Mismatch::Invented`]: an entry is a set of overrides by construction
/// (`agents`' `[display.<name>]` documents all three of its keys as optional),
/// so "absent" is a value rather than a disagreement.
fn check_record(path: &str, fields: &[Field], item: &toml_edit::Item, out: &mut Vec<Mismatch>) {
    let Some(table) = item.as_table_like() else {
        out.push(Mismatch::BadDefault {
            path: path.to_owned(),
            found: spell(item),
            expected: "a table".to_owned(),
        });
        return;
    };
    for (key, value) in table.iter() {
        if key == merge::UNSET_KEY || key == merge::LOCKED_KEY {
            continue;
        }
        match fields.iter().find(|field| field.path == key) {
            Some(field) => check_item(&format!("{path}.{key}"), &field.kind, value, out),
            None => out.push(Mismatch::Forgot {
                path: format!("{path}.{key}"),
            }),
        }
    }
}

/// Whether a scalar `value` fits a scalar `kind`. The collection kinds are
/// handled structurally by [`check_item`] and answer `false` here, which is
/// unreachable from it and is the conservative answer anywhere else.
fn fits(kind: &Kind, value: &toml_edit::Value) -> bool {
    match kind {
        Kind::Bool => value.as_bool().is_some(),
        Kind::Int { min, max } => value
            .as_integer()
            .is_some_and(|n| (*min..=*max).contains(&n)),
        Kind::Choice { options } => value.as_str().is_some_and(|s| options.contains(&s)),
        Kind::Text { blank_ok } => value
            .as_str()
            .is_some_and(|s| *blank_ok || !s.trim().is_empty()),
        Kind::Color { options } => value
            .as_str()
            .is_some_and(|s| options.contains(&s) || is_hex_rgb(s)),
        Kind::List(_) | Kind::Map(_) => false,
    }
}

/// `#rrggbb` or a bare `rrggbb`, case-insensitive — the literal arm of
/// [`Kind::Color`], mirroring `core-leds`' own `parse_hex_rgb`.
fn is_hex_rgb(raw: &str) -> bool {
    let hex = raw.strip_prefix('#').unwrap_or(raw);
    hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A [`Mismatch::BadDefault`] naming what was found and what was wanted.
fn bad(path: &str, item: &toml_edit::Item, kind: &Kind) -> Mismatch {
    Mismatch::BadDefault {
        path: path.to_owned(),
        found: spell(item),
        expected: kind.expected(),
    }
}

/// How a rejected value is quoted back at its author: the bytes it has in the
/// file, trimmed of the decor `toml_edit` hangs on either side of them.
fn spell(item: &toml_edit::Item) -> String {
    item.as_value().map_or_else(
        || "a table".to_owned(),
        |value| value.to_string().trim().to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── A fixture family, and one deliberate mismatch of each class ──────────
    //
    // A fixture rather than one of the four real families: the real ones are
    // pinned by their own `verify` tests in their own crates, and a test that
    // proves the walker *catches* something has to be able to break one side of
    // the pair on purpose.

    const FIXTURE_TOML: &str = r##"# a fixture family
flag = true
count = 7
word = "left"
label = "hello"
tint = "#ff8800"

[nest]
inner = false
"##;

    const NEST_FIELDS: &[Field] = &[Field {
        path: "inner",
        kind: Kind::Bool,
        doc: "an entry key",
    }];

    const FIELDS: &[Field] = &[
        Field {
            path: "flag",
            kind: Kind::Bool,
            doc: "a boolean",
        },
        Field {
            path: "count",
            kind: Kind::Int { min: 1, max: 10 },
            doc: "a bounded integer",
        },
        Field {
            path: "word",
            kind: Kind::Choice {
                options: &["left", "right"],
            },
            doc: "a fixed vocabulary",
        },
        Field {
            path: "label",
            kind: Kind::Text { blank_ok: false },
            doc: "free text",
        },
        Field {
            path: "tint",
            kind: Kind::Color {
                options: &["heat", "style"],
            },
            doc: "a colour",
        },
        Field {
            path: "nest.inner",
            kind: Kind::Bool,
            doc: "a key inside a plain table",
        },
    ];

    const FIXTURE: Schema = Schema {
        family: "fixture",
        fields: FIELDS,
    };

    fn verify_err(schema: &Schema, toml: &str) -> Vec<Mismatch> {
        verify(schema, toml).expect_err("this fixture is supposed to disagree")
    }

    #[test]
    fn a_schema_that_matches_its_default_verifies() {
        verify(&FIXTURE, FIXTURE_TOML).expect("the fixture schema matches the fixture default");
    }

    #[test]
    fn a_leaf_the_schema_forgot_is_reported_by_path() {
        // One class, one break: the default grows a key nobody declared.
        // Spliced in front of `[nest]`, because a line after a table header
        // belongs to that table and would be `nest.extra`.
        let toml = FIXTURE_TOML.replace("[nest]", "extra = 3\n\n[nest]");
        assert_eq!(
            verify_err(&FIXTURE, &toml),
            vec![Mismatch::Forgot {
                path: "extra".to_owned()
            }],
        );
    }

    #[test]
    fn a_leaf_inside_a_plain_table_is_reported_with_its_dotted_path() {
        let toml = FIXTURE_TOML.replace("inner = false", "inner = false\nmystery = 1");
        assert_eq!(
            verify_err(&FIXTURE, &toml),
            vec![Mismatch::Forgot {
                path: "nest.mystery".to_owned()
            }],
        );
    }

    #[test]
    fn a_field_the_default_does_not_state_is_invented() {
        const INVENTED: &[Field] = &[Field {
            path: "ghost",
            kind: Kind::Bool,
            doc: "declared, never documented",
        }];
        const SCHEMA: Schema = Schema {
            family: "fixture",
            fields: INVENTED,
        };
        assert_eq!(
            verify_err(&SCHEMA, "# nothing at all\n"),
            vec![Mismatch::Invented {
                path: "ghost".to_owned()
            }],
        );
    }

    #[test]
    fn a_default_outside_its_kind_is_reported_with_both_halves() {
        // An integer outside `min..=max` — the shape a range typo takes.
        let toml = FIXTURE_TOML.replace("count = 7", "count = 70");
        assert_eq!(
            verify_err(&FIXTURE, &toml),
            vec![Mismatch::BadDefault {
                path: "count".to_owned(),
                found: "70".to_owned(),
                expected: "a whole number, 1..=10".to_owned(),
            }],
        );
    }

    #[test]
    fn a_wrong_type_reads_like_any_other_bad_default() {
        let toml = FIXTURE_TOML.replace("flag = true", "flag = 5");
        assert_eq!(
            verify_err(&FIXTURE, &toml),
            vec![Mismatch::BadDefault {
                path: "flag".to_owned(),
                found: "5".to_owned(),
                expected: "true or false".to_owned(),
            }],
        );
    }

    #[test]
    fn a_word_outside_the_choice_is_a_bad_default() {
        let toml = FIXTURE_TOML.replace(r#"word = "left""#, r#"word = "sideways""#);
        assert_eq!(
            verify_err(&FIXTURE, &toml),
            vec![Mismatch::BadDefault {
                path: "word".to_owned(),
                found: "\"sideways\"".to_owned(),
                expected: "one of left, right".to_owned(),
            }],
        );
    }

    #[test]
    fn a_colour_takes_a_name_or_a_hex_literal_and_nothing_else() {
        for good in ["\"heat\"", "\"style\"", "\"#ff8800\"", "\"FF8800\""] {
            let toml = FIXTURE_TOML.replace(r##"tint = "#ff8800""##, &format!("tint = {good}"));
            verify(&FIXTURE, &toml).unwrap_or_else(|e| panic!("{good} should verify: {e:?}"));
        }
        let toml = FIXTURE_TOML.replace(r##"tint = "#ff8800""##, r##"tint = "#ff88""##);
        assert_eq!(
            verify_err(&FIXTURE, &toml),
            vec![Mismatch::BadDefault {
                path: "tint".to_owned(),
                found: "\"#ff88\"".to_owned(),
                expected: "one of heat, style, or an #rrggbb literal".to_owned(),
            }],
        );
    }

    #[test]
    fn a_blank_text_default_is_refused_unless_blank_is_allowed() {
        const LOOSE: &[Field] = &[Field {
            path: "label",
            kind: Kind::Text { blank_ok: true },
            doc: "free text, blank allowed",
        }];
        const SCHEMA: Schema = Schema {
            family: "fixture",
            fields: LOOSE,
        };

        let toml = FIXTURE_TOML.replace(r#"label = "hello""#, r#"label = "   ""#);
        assert_eq!(verify_err(&FIXTURE, &toml).len(), 1);
        verify(&SCHEMA, "label = \"\"\n").expect("blank_ok accepts a blank default");
    }

    #[test]
    fn every_class_is_reported_at_once_rather_than_only_the_first() {
        const WITH_GHOST: &[Field] = &[
            Field {
                path: "count",
                kind: Kind::Int { min: 1, max: 10 },
                doc: "a bounded integer",
            },
            Field {
                path: "ghost",
                kind: Kind::Bool,
                doc: "declared, never documented",
            },
        ];
        const SCHEMA: Schema = Schema {
            family: "fixture",
            fields: WITH_GHOST,
        };

        let toml = FIXTURE_TOML
            .replace("count = 7", "count = 70")
            .replace("[nest]", "extra = 3\n\n[nest]");
        let found: Vec<String> = verify_err(&SCHEMA, &toml)
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            found.len(),
            8,
            "one per undeclared leaf, plus the two breaks: {found:?}"
        );
        assert!(found.iter().any(|m| m.starts_with("count defaults to 70")));
        assert!(found.iter().any(|m| m.starts_with("extra is stated")));
        assert!(found.iter().any(|m| m.starts_with("ghost is declared")));
    }

    // ── The collection kinds ─────────────────────────────────────────────────

    #[test]
    fn a_collection_the_default_does_not_state_is_not_invented() {
        // `workspaces.toml`'s shape: a comments-only default, a `List` and a
        // `Map` declared over it.
        const COLLECTIONS: &[Field] = &[
            Field {
                path: "order",
                kind: Kind::List(&Kind::Text { blank_ok: false }),
                doc: "a list",
            },
            Field {
                path: "nest",
                kind: Kind::Map(NEST_FIELDS),
                doc: "a map",
            },
        ];
        const SCHEMA: Schema = Schema {
            family: "fixture",
            fields: COLLECTIONS,
        };
        verify(&SCHEMA, "# nothing but comments\n")
            .expect("an absent collection is not an invented field");
    }

    #[test]
    fn a_map_entry_is_checked_against_the_sub_fields() {
        const FIELDS: &[Field] = &[Field {
            path: "nest",
            kind: Kind::Map(NEST_FIELDS),
            doc: "a map",
        }];
        const SCHEMA: Schema = Schema {
            family: "fixture",
            fields: FIELDS,
        };
        verify(&SCHEMA, "[nest.one]\ninner = true\n").expect("a good entry verifies");

        assert_eq!(
            verify_err(&SCHEMA, "[nest.one]\ninner = 5\n"),
            vec![Mismatch::BadDefault {
                path: "nest.one.inner".to_owned(),
                found: "5".to_owned(),
                expected: "true or false".to_owned(),
            }],
        );
        assert_eq!(
            verify_err(&SCHEMA, "[nest.one]\nmystery = true\n"),
            vec![Mismatch::Forgot {
                path: "nest.one.mystery".to_owned()
            }],
        );
    }

    #[test]
    fn a_map_entry_missing_a_sub_field_is_fine() {
        const FIELDS: &[Field] = &[Field {
            path: "nest",
            kind: Kind::Map(NEST_FIELDS),
            doc: "a map",
        }];
        const SCHEMA: Schema = Schema {
            family: "fixture",
            fields: FIELDS,
        };
        verify(&SCHEMA, "[nest.one]\n").expect("an entry carries overrides, not every key");
    }

    #[test]
    fn a_list_of_records_checks_each_element_as_a_record() {
        const APP: &[Field] = &[
            Field {
                path: "id",
                kind: Kind::Text { blank_ok: false },
                doc: "an id",
            },
            Field {
                path: "exec",
                kind: Kind::Text { blank_ok: false },
                doc: "an override",
            },
        ];
        const ENTRY: Kind = Kind::Map(APP);
        const FIELDS: &[Field] = &[Field {
            path: "apps",
            kind: Kind::List(&ENTRY),
            doc: "a list of records",
        }];
        const SCHEMA: Schema = Schema {
            family: "fixture",
            fields: FIELDS,
        };
        verify(
            &SCHEMA,
            "apps = [ { id = \"a\" }, { id = \"b\", exec = \"b -x\" } ]\n",
        )
        .expect("a list of records verifies element by element");
        assert_eq!(
            verify_err(&SCHEMA, "apps = [ { id = 5 } ]\n"),
            vec![Mismatch::BadDefault {
                path: "apps[0].id".to_owned(),
                found: "5".to_owned(),
                expected: "text that is not blank".to_owned(),
            }],
        );
    }

    #[test]
    fn a_list_whose_default_is_not_an_array_is_a_bad_default() {
        const FIELDS: &[Field] = &[Field {
            path: "order",
            kind: Kind::List(&Kind::Text { blank_ok: false }),
            doc: "a list",
        }];
        const SCHEMA: Schema = Schema {
            family: "fixture",
            fields: FIELDS,
        };
        assert_eq!(
            verify_err(&SCHEMA, "order = \"chat\"\n"),
            vec![Mismatch::BadDefault {
                path: "order".to_owned(),
                found: "\"chat\"".to_owned(),
                expected: "an array of (text that is not blank)".to_owned(),
            }],
        );
    }

    // ── The edges ────────────────────────────────────────────────────────────

    #[test]
    fn the_reserved_markers_are_not_leaves() {
        // `_unset` and `_locked` are the merge's, not a family's — and a
        // schema that had to declare them would be declaring a key the reader
        // strips before the type ever sees it.
        let toml = format!("{FIXTURE_TOML}_unset = [\"flag\"]\n_locked = [\"count\"]\n");
        verify(&FIXTURE, &toml).expect("markers are the merge's own keys");
    }

    #[test]
    fn a_default_that_is_not_toml_is_reported_rather_than_panicking() {
        let found = verify_err(&FIXTURE, "flag = = true\n");
        assert!(
            matches!(found.as_slice(), [Mismatch::Unparsable { .. }]),
            "{found:?}"
        );
    }

    #[test]
    fn accepts_answers_exactly_what_verify_would() {
        let kind = Kind::Int { min: 1, max: 60 };
        assert!(kind.accepts(&toml_edit::Value::from(1_i64)));
        assert!(kind.accepts(&toml_edit::Value::from(60_i64)));
        assert!(!kind.accepts(&toml_edit::Value::from(0_i64)));
        assert!(!kind.accepts(&toml_edit::Value::from(61_i64)));
        assert!(!kind.accepts(&toml_edit::Value::from("many")));

        let colour = Kind::Color { options: &["heat"] };
        assert!(colour.accepts(&toml_edit::Value::from("heat")));
        assert!(colour.accepts(&toml_edit::Value::from("#0a0B0c")));
        assert!(!colour.accepts(&toml_edit::Value::from("#0a0b0")));
    }

    #[test]
    fn schema_field_finds_a_top_level_path_and_only_that() {
        assert_eq!(
            FIXTURE.field("nest.inner").map(|f| f.kind),
            Some(Kind::Bool)
        );
        assert_eq!(FIXTURE.field("nest").map(|f| f.kind), None);
    }
}
