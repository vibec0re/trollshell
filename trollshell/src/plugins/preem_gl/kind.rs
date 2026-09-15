//! Which preem kit widgets have a **GL arm** — the one enumeration #1211 ties
//! every per-kind GL list to.
//!
//! Before #1211, six lists each named the same five kinds independently, with
//! nothing tying them together: [`super::install`]'s five `register` calls;
//! `Renderer::matches_kind` and `Renderer::invalidate_cached_frames`
//! (`preem_render.rs`); [`Kind`] itself plus its `pinned_exact`/
//! `checks_peak_rows`/`edge_budget` answers (`parity.rs`); the parity
//! harness's case list (`preem_gl::cases::cases_for`, `#[path]`-included by
//! `examples/preem_gl_diff.rs`); and the case count
//! `nix/checks/system-tests.nix` asserts. A sixth kind could land in any one
//! of those and ship with zero parity coverage everywhere else — all green,
//! as the #1209 review (LOW-1) measured.
//!
//! [`Kind::ALL`] is what the others now derive from or are tested against:
//! [`super::install`] loops over it and [`Kind::gl_seam`] to build its
//! registration list, so a new variant is registered the moment it answers
//! there; `plugins::tests`' `kind_enumeration` module walks it to assert a
//! `gl_seam_for` result, an `invalidate_cached_frames` decision and at least
//! one harness case per skin exist for every kind, and that the harness's
//! computed case count matches `nix/checks/system-tests.nix`'s literal; and
//! `parity.rs`'s own tests pair it with the exhaustive `pinned_exact`/
//! `checks_peak_rows`/`edge_budget` matches, as they did before this file
//! existed. Add a variant to [`Kind::ALL`] without answering all of those and
//! something fails to compile or a test reds — that is the property this
//! module exists for.

use hytte::ui::gl_surface::{GlPipeline, GlProgram};

use super::{dot_matrix, flip_board, gauge, led_strip, marquee, program, seven_seg, textbox};

/// Which kit widget a case is measuring, because the two do **not** take the
/// same structural checks (#1143).
///
/// The ceiling (mean ≤ 2 / p99 ≤ 8 / max ≤ 32 per channel) is #893's, and it
/// is what Annika agreed to on that thread — it is what every real driver
/// faces, and nothing here touches it. What `TROLLSHELL_PARITY_EXACT=1` adds
/// on top is a *measurement*, not a design target: it is exported in exactly
/// one place, `nix/checks/system-tests.nix`, inside the llvmpipe sandbox, so
/// it is a regression detector for one pinned software rasteriser and costs
/// nothing on hardware. See `parity.rs`'s `pinned_exact`/`checks_peak_rows`/
/// `edge_budget` for the per-kind answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// `preem.scope` — pinned bit-exact under `TROLLSHELL_PARITY_EXACT=1`, and
    /// structurally checked with the per-column peak-row test.
    Scope,
    /// `preem.gauge` (#1143) — pinned bit-exact too, since #1148's review; no
    /// peak-row check, which is a beam statistic.
    Gauge,
    /// `preem.dot_matrix` (#1144) — **pinned bit-exact**, like the scope, and
    /// for the scope's reason rather than in spite of Annika's.
    ///
    /// Her word on #865 is about **glass**, and the ceiling is what carries it:
    /// a real driver only ever has to clear mean 2 / p99 8 / max 32, and
    /// nothing here tightens that. `TROLLSHELL_PARITY_EXACT=1` is exported in
    /// exactly one place — `nix/checks/system-tests.nix`, inside a sandbox with
    /// Mesa llvmpipe — so what it pins is a *CI regression detector* against
    /// one known driver, not a design target.
    ///
    /// All sixteen dot-matrix cases measured **max |Δ| 0** of 255 on every
    /// channel there, which makes the pin available; and it is worth taking
    /// precisely because this arm's improvement is a coordinate change. At 1:1
    /// the shader snaps its sample to the pixel centre so the continuous
    /// falloff collapses onto the kit's integer table — a collapse that is
    /// exact by construction and would stop being exact silently. Zero is the
    /// only value that can say so.
    DotMatrix,
    /// `preem.marquee` (#1152) — **pinned bit-exact**, and it inherits nothing
    /// to say so.
    ///
    /// It runs the dot matrix's shader, so the *reason* zero is available is
    /// that one (the 1:1 snap collapses the continuous falloff onto the kit's
    /// integer table by construction) — but the geometry it collapses onto is
    /// this widget's own: a continuous ticker grid, centred in its window
    /// rather than inset by the bezel, addressed through three uniforms the dot
    /// matrix drives with different numbers. All twenty 1:1 marquee cases
    /// measured **max |Δ| 0** of 255 on every channel under llvmpipe, at five
    /// scroll phases across four skins, which is what makes the pin a
    /// measurement rather than an inheritance.
    Marquee,
    /// `preem.textbox` (#1152) — **pinned bit-exact**, and the easiest of the
    /// five to hold there.
    ///
    /// There is no falloff, no bloom and no CRT comb in this widget: the kit
    /// `set`s flat bytes and never composites, so the shader has nothing to
    /// round. The one place it could disagree is the rounded corner, where the
    /// continuous `poke` is written with the ±0.5 shifts that turn it back into
    /// the kit's integer `corner_delta` at a logical pixel centre — and that is
    /// exactly the collapse a pin is worth taking to protect. All sixteen 1:1
    /// cases measured max |Δ| 0 on every channel.
    TextBox,
    /// `preem.led_strip` (#1153) — **pinned bit-exact**, and the one kind here
    /// whose pin is also asserted *hermetically*.
    ///
    /// Its geometry is integers all the way down — the segment bounds, the
    /// bezel and the row band are whole buffer pixels, and the bloom window's
    /// half-width is a half-integer — so at a pixel centre the coverage
    /// integral is exactly `0` or `1` and the closed-form blur's two measures
    /// are exactly the integer column and row counts the kit sums. Zero is
    /// therefore available by construction, and it is what all twenty 1:1 cases
    /// measured under llvmpipe.
    ///
    /// What makes this one different is `led_strip.rs`'s
    /// `the_transcribed_shader_is_bit_exact_against_the_kit_at_one_to_one`: the
    /// shader's arithmetic is mirrored in Rust, held to the shipped GLSL by a
    /// source scan, and compared against the kit's own bytes on every skin in
    /// `cargo test`. The pin here still says what the *driver* did; that one
    /// says the arithmetic being pinned was right before a driver ever saw it.
    LedStrip,
    /// `preem.seven_seg` (#1154) — **pinned bit-exact**, and the second kind
    /// whose pin is also asserted *hermetically*.
    ///
    /// Its segments are tapered hexagons, and this arm draws the **chamfer**
    /// the kit's six-row staircase samples. That collapse is what the pin is
    /// worth taking to protect: at a pixel centre each of the shape's three
    /// half-plane residuals is a non-zero half-integer, so the point test
    /// returns exactly `stamp_bar`'s integer range test and the two arms draw
    /// the same picture — by construction, and silently if the construction
    /// ever stopped holding. Zero is the only value that can say so.
    ///
    /// Like [`LedStrip`](Self::LedStrip), `seven_seg.rs`'s
    /// `the_transcribed_shader_is_bit_exact_against_the_kit_at_one_to_one`
    /// mirrors the shader's arithmetic — this one including `blur.frag`, since
    /// unlike the meter this widget has a real blur pass — holds it to the
    /// shipped GLSL by a source scan, and compares it against the kit's own
    /// bytes on every skin in `cargo test`. The pin here still says what the
    /// *driver* did; that one says the arithmetic being pinned was right before
    /// a driver ever saw it.
    SevenSeg,
    /// `preem.flip_board` (#1155) — **pinned bit-exact**, and the third kind
    /// whose pin is also asserted *hermetically*.
    ///
    /// The first kind on this seam whose 1:1 branch is **not** a point test.
    /// The scope, the dot matrix, the meter and the readout all collapse onto
    /// an integer at a pixel centre; a flip board does not, because the kit
    /// itself takes a fractional area average there — the falling card's fold
    /// is an exact coverage-weighted resample of the source rows a destination
    /// row now spans, and the kit computes it in `f32`. So this arm's 1:1
    /// branch reproduces float arithmetic rather than collapsing out of it,
    /// which makes the pin a statement about **operation order and rounding**
    /// and not about a degeneracy.
    ///
    /// That is also why it is worth having: two of the operations are
    /// divisions, and GLSL ES 3.20 §4.7.1 allows `a / b` **2.5 ULP** where it
    /// pins `+`, `-` and `*` to a correctly rounded result (#1309's lesson).
    /// Both divides are the *kit's* — `(lo - band_lo) / squash` and
    /// `acc / span`, spelled by `FlipBoard::compose_flap` — so they cannot be
    /// moved to a CPU-computed reciprocal without making the two arms disagree.
    /// `flip_board.rs`'s
    /// `the_coverage_bytes_are_never_decided_by_the_divides_slack` censuses
    /// exactly that: every byte of every 1:1 case re-derived with both
    /// quotients perturbed by ±2.5 ULP, with a negative control at a millionth
    /// that moves bytes.
    ///
    /// Like [`LedStrip`](Self::LedStrip) and [`SevenSeg`](Self::SevenSeg),
    /// `flip_board.rs`'s
    /// `the_transcribed_shader_is_bit_exact_against_the_kit_at_one_to_one`
    /// mirrors the shader's arithmetic — this one including `blur.frag` twice
    /// over, since a nixie blooms twice — holds it to the shipped GLSL by a
    /// source scan, and compares it against the kit's own bytes in
    /// `cargo test`. The pin here still says what the *driver* did; that one
    /// says the arithmetic being pinned was right before a driver ever saw it.
    FlipBoard,
}

impl Kind {
    /// Every kind with a GL arm — the enumeration #1211 ties every per-kind
    /// list to, one way or another.
    ///
    /// It has to be extended by hand when a kind lands — but forgetting to is
    /// a **compile** error rather than a silent gap. [`super::install`] loops
    /// over this array and [`Kind::gl_seam`] to register every pipeline, so a
    /// variant missing here is a chip nothing ever registers; `parity.rs`'s
    /// `every_kind_states_its_pin_and_its_beam_check` pairs this list with an
    /// exhaustive `match` over the enum, so a variant missing from here still
    /// has to be named there and a variant named there still has to be here
    /// for the loop to reach it; and `plugins::tests`' `kind_enumeration`
    /// walks it against `preem_gl::cases::cases_for`'s real output, so a kind
    /// with no harness case fails there instead of shipping unnoticed.
    ///
    /// Not `#[cfg(test)]` any more (#1211): before this module existed, the
    /// shell mounted `Kind` under `cfg(test)` because the tests were its only
    /// consumer, so the constant was gated the same way to keep it from being
    /// an unused-in-production warning. [`super::install`] is now a
    /// consumer too, so the constant has to exist in every build.
    pub(crate) const ALL: [Self; 8] = [
        Self::Scope,
        Self::Gauge,
        Self::DotMatrix,
        Self::Marquee,
        Self::TextBox,
        Self::LedStrip,
        Self::SevenSeg,
        Self::FlipBoard,
    ];

    /// The `(program, pipeline)` pair [`super::install`] registers for this
    /// kind.
    ///
    /// An exhaustive `match`, deliberately, and the reason [`super::install`]
    /// can be a loop over [`ALL`](Self::ALL) rather than five hand-written
    /// `register` calls: a sixth kind added to [`ALL`](Self::ALL) without a
    /// matching arm here fails to *compile*, rather than shipping a chip
    /// nothing ever registers with `hytte-ui`.
    ///
    /// The marquee's pair is the dot matrix's own pipeline under its own
    /// program name — see `preem_gl::marquee`'s module docs for why a ticker
    /// is the same dot hardware on a different grid.
    pub(crate) fn gl_seam(self) -> (GlProgram, GlPipeline) {
        match self {
            Self::Scope => (program::SCOPE, program::SCOPE_PIPELINE),
            Self::Gauge => (gauge::GAUGE, gauge::GAUGE_PIPELINE),
            Self::DotMatrix => (dot_matrix::DOT_MATRIX, dot_matrix::DOT_MATRIX_PIPELINE),
            Self::Marquee => (marquee::MARQUEE, marquee::MARQUEE_PIPELINE),
            Self::TextBox => (textbox::TEXTBOX, textbox::TEXTBOX_PIPELINE),
            Self::LedStrip => (led_strip::LED_STRIP, led_strip::LED_STRIP_PIPELINE),
            Self::SevenSeg => (seven_seg::SEVEN_SEG, seven_seg::SEVEN_SEG_PIPELINE),
            Self::FlipBoard => (flip_board::FLIP_BOARD, flip_board::FLIP_BOARD_PIPELINE),
        }
    }
}
