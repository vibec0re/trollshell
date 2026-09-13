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

use super::{dot_matrix, gauge, marquee, program, textbox};

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
    pub(crate) const ALL: [Self; 5] = [
        Self::Scope,
        Self::Gauge,
        Self::DotMatrix,
        Self::Marquee,
        Self::TextBox,
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
        }
    }
}
