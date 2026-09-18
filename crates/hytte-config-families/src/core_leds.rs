//! `core-leds.toml` — the Stats drawer's per-core LED panel (#857/#869).
//!
//! The documented default and the schema; the parsing, the service and the
//! live reload stay in the shell (`trollshell/src/config/core_leds.rs`), which
//! re-exports both consts from their old paths so nothing that reads them
//! moved.

use hytte_config::schema::{Field, Kind, Schema};

use crate::Family;

/// The family, for [`crate::FAMILIES`].
pub const FAMILY: Family = Family {
    name: "core-leds",
    schema: &SCHEMA,
    default_toml: DEFAULT_TOML,
};

/// The four leaves of `core-leds.toml`.
///
/// Every vocabulary here is spelled as a **literal** rather than read out of
/// `hytte-preem`'s `DisplayStyle::ALL` / `ColorMap::ALL`, because this crate
/// deliberately depends on nothing but `hytte-config` (see the crate docs) —
/// and because a `&'static [&'static str]` cannot be built from an array of
/// enums in a `const` anyway. The literals are held to the kit by
/// `the_schema_matches_the_documented_default`'s siblings in the *shell*
/// crate, which links `hytte-preem` and asserts each list equals the enum's
/// own names in the enum's own order.
pub const SCHEMA: Schema = Schema {
    family: "core-leds",
    fields: FIELDS,
};

const FIELDS: &[Field] = &[
    Field {
        path: "style",
        // `DisplayStyle::ALL`, in `name()`'s spelling and `ALL`'s order.
        kind: Kind::Choice {
            options: &["vfd", "lcd", "oled", "crt"],
        },
        doc: "The kit skin — the panel's physical character.",
    },
    Field {
        path: "color",
        // `ColorMap::ALL` (which does not include the `Rgb` literal arm — that
        // is what `Kind::Color`'s own `#rrggbb` half is), in `ALL`'s order.
        kind: Kind::Color {
            options: &["style", "rainbow", "transpride", "heat"],
        },
        doc: "The colour axis, independent of the skin.",
    },
    Field {
        path: "rows",
        // `0` is the automatic wide rectangle and `64` is `MAX_ROWS` — but the
        // word `"rect"` names the same shape as `0`, and that is not a
        // curiosity of the parser: `ROWS.file_accepts` documents it and
        // `nix/module-common.nix` renders it (`either (ints.between 0 64)
        // (enum [ "rect" ])`), so a base layer can put the word in front of
        // this row. Without the `also` arm `Kind::accepts` — the form's
        // validator — would disagree with the loader on the one value a nix
        // base actually writes (#1360 review, HIGH 2).
        kind: Kind::Int {
            min: 0,
            max: 64,
            also: &["rect"],
        },
        doc: "Rows in the lamp matrix, or 0 / \"rect\" for the automatic wide rectangle picked from the core count.",
    },
    Field {
        path: "fill",
        // `parse_core_leds_fill`'s match arms.
        kind: Kind::Choice {
            options: &["spare", "blank"],
        },
        doc: "What a ragged last row's leftover slots look like.",
    },
];

/// The documented default, and the bottom merge layer.
///
/// Kept commented because it is the only place a key is explained, it is what
/// an operator sees when they first open their overlay, and it is parsed on
/// every load — so a syntax error in it fails any test that loads the
/// subsystem rather than surfacing in production.
pub const DEFAULT_TOML: &str = r##"# The Stats drawer's per-core LED panel (#857): one lamp per CPU core, each
# lit to that core's load. Every key here is look-and-feel — none of it
# changes what is measured.
#
# This file is read live: save an edit and the panel re-skins within a few
# seconds, with no shell restart. It is also layered — a nix-written base
# under $XDG_CONFIG_DIRS, your own edits under $XDG_CONFIG_HOME — so a
# rebuild never clobbers a hand edit and a hand edit never blocks a rebuild.
# Delete a key to fall back to the value below; `_unset = ["style"]` erases a
# key an underlying layer set (TOML has no null, so this is how it is spelt).
#
# A value no parser accepts costs its own key and nothing else: that key takes
# the built-in default, one journal line names it, and every other key in the
# file still applies.

# The kit skin — the panel's physical character.
#   vfd         near-black field with a phosphor halo off every lit lamp
#   lcd         grey-green cells that ghost their unlit segments
#   oled        pure black field, no ghost
#   crt         scanline comb and a curved-glass vignette
style = "vfd"

# The colour axis, and it is *independent* of the skin: style = "crt" with
# color = "heat" gives heat-mapped lamps through the tube's scanlines.
#   heat        blue-to-red ramp by load — "which core is busy", at a glance
#   style       the skin's own single ink (the pre-#857 look)
#   rainbow     a hue sweep across the lamps
#   transpride  the flag's bands across the lamps
#   "#rrggbb"   one literal colour
color = "heat"

# Rows in the lamp matrix. 0 — or the word "rect", which is what the deprecated
# TROLLSHELL_CORE_LEDS_ROWS took, and which this key accepts too — is the
# automatic wide rectangle, picked from the core count (16x4 on a 64-thread
# box, 4x1 at 4 cores). Any number from 1 to 64 pins the row count instead and
# the columns fall out of it; past 64 a row is thinner than a pixel on screen,
# so it is rejected as the typo it almost certainly is.
rows = 0

# What a ragged last row's leftover slots look like. Only visible when the row
# count divides unevenly *and* the skin ghosts (vfd/lcd).
#   spare       unlit lamps fill the tail
#   blank       the tail is left bare
fill = "spare"
"##;

#[cfg(test)]
mod tests {
    use super::*;

    /// The walker, on the family it describes.
    ///
    /// The shell's own copy of this test is the one that also holds the
    /// vocabularies to `hytte-preem`'s enums; this one is here so the leaf
    /// crate is not a pile of consts nothing checks.
    #[test]
    fn the_schema_matches_the_documented_default() {
        hytte_config::schema::verify(&SCHEMA, DEFAULT_TOML)
            .expect("core-leds' schema and its documented default must agree");
    }
}
