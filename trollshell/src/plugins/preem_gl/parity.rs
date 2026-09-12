//! The parity harness's arithmetic: the GL↔CPU delta statistics and the
//! verdict they feed (#893 stage B).
//!
//! # Why this is a module in the shell and not lines in the example
//!
//! `trollshell/examples/preem_gl_diff.rs` pulls this in with `#[path]`, the
//! same way it pulls in [`program`](super::program). It could have been written
//! inline there — and was — but **`cargo test` does not run `#[test]`s inside
//! an example**: examples default to `test = false`, so a test module in one
//! compiles under `--all-targets` and is never executed. A statistic that
//! decides whether #893's ceiling is met, guarded by a test that silently never
//! runs, is worse than no test at all.
//!
//! So it lives here, `#[cfg(test)]`-mounted by [`super`], which puts its tests
//! in `cargo test -p trollshell --lib` where they actually run. The shell
//! itself never calls any of this: the *shell* must never read a framebuffer
//! back — that is a full pipeline stall per chip per frame, the cost #863 set
//! out to remove — which is exactly why the harness is an example and why the
//! shell links no GL crate outside its dev-dependencies.
//!
//! Like `program`, this module references nothing above it.
//!
//! # The statistic is **per channel**
//!
//! Annika's answer 4 on #893 set the ceiling at mean 2 / p99 8 / max 32 **per
//! channel**, and one lumped R+G+B population is not that: it divides a
//! single-channel drift by three. The CRT skin's ink is `[0x5c, 0xff, 0x82]`,
//! so a green-only error of 6/255 lumps to a mean of 2 and passes a ceiling it
//! misses by three times. Each channel gets its own population here and the
//! verdict takes the **worst** of the three.

/// Per-channel ceiling on the mean absolute difference, in 255ths (#893,
/// Annika's answer 4).
pub(crate) const CEILING_MEAN: f64 = 2.0;
/// Per-channel ceiling on the 99th percentile.
pub(crate) const CEILING_P99: f64 = 8.0;
/// Per-channel ceiling on the single worst pixel.
pub(crate) const CEILING_MAX: f64 = 32.0;

/// Channel names, for the transcript.
pub(crate) const CHANNELS: [&str; 3] = ["R", "G", "B"];

/// Ceiling on the **mean** |Δ| over the edge region of a supersampled case
/// (#1148 review, HIGH-2), in 255ths.
///
/// A separate budget from #893's because it is a different measurement, and
/// saying so out loud is the point. #893's ceiling bounds two renderers drawing
/// the same picture at the same resolution, where a disagreement is rounding. A
/// supersampled case compares a box-average of a **twice-as-dense** render
/// against a single-sample one: on every pixel the kit anti-aliased, the two
/// have genuinely different coverage, and that difference *is* the improvement
/// #1090 asked for. Holding it to mean 2 would be asking the fix not to happen —
/// measured on llvmpipe, the four shipping-scale cases come out at an edge mean
/// of 6.1 to 9.4 with the whole rest of the frame bit-identical.
///
/// So this bounds the drift rather than the difference: 16 is a little under
/// twice the worst measured, which leaves a driver room to disagree about a
/// ramp and leaves none for a face drawn in a different place. The assertion
/// with the teeth is [`Regions::interior_max`] — see [`case_verdict`].
pub(crate) const SUPERSAMPLED_EDGE_MEAN: f64 = 16.0;

/// Ceiling on the **single worst** edge pixel of a supersampled case, in
/// 255ths. Measured worst on llvmpipe: 76.
///
/// Half of full contrast. An anti-aliased edge pixel can legitimately be most
/// of the way from field to ink in one render and most of the way back in the
/// other — that is what a one-pixel ramp against a half-pixel ramp *is* — but a
/// pixel that swings further than half the palette's range is not reporting a
/// ramp any more.
pub(crate) const SUPERSAMPLED_EDGE_MAX: u8 = 128;

/// How far a column's brightest row may move, in **grid** rows, before it counts
/// as a structural difference rather than a rounding one.
///
/// **One**, and the reason is the kit's kernel rather than its span. `GLOW` is a
/// bright `CORE` with strictly dimmer steps either side (`hytte-preem`'s
/// `scope.rs`: 255 / 130 / 45), so the brightest row of a beam is always its
/// core row — the two glow rows on each side can never *be* the peak, and the
/// beam being five rows tall does not widen the peak's legitimate travel by one
/// row. What can legitimately move it is the single disagreement this harness
/// exists to bound: `round()` at an exact `.5`, which lands a column one row
/// either way. Two rows would already be a beam drawn somewhere else.
pub(crate) const PEAK_ROW_TOLERANCE_GRID_ROWS: usize = 1;

/// [`PEAK_ROW_TOLERANCE_GRID_ROWS`] converted into the units the comparison
/// actually runs in: **reference-frame pixels**, which are the kit's grid rows
/// times its own `scale`.
///
/// There is deliberately **no device-scale term**. Both peak rows come out of
/// `compare`, which walks `0..ref_h` and whose `gl_at` has already divided the
/// device scale back out — so both are reference rows, and multiplying the
/// tolerance by the device scale again would make `FAIL(beam)` depend on which
/// monitor the harness was run on (2 px on a 1× screen, 4 px on a 2× one, same
/// pixels and same pipeline). That is what this function exists to make
/// unsayable at the call site.
pub(crate) fn peak_row_tolerance(upscale: usize) -> usize {
    PEAK_ROW_TOLERANCE_GRID_ROWS * upscale.max(1)
}

/// One channel's absolute-difference distribution.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct ChannelStats {
    /// Mean |Δ| over every compared pixel, in 255ths.
    pub(crate) mean: f64,
    /// 99th percentile of |Δ|.
    pub(crate) p99: f64,
    /// The single worst pixel's |Δ|.
    pub(crate) max: f64,
}

impl ChannelStats {
    /// Whether this channel is inside the proposed ceiling.
    pub(crate) fn inside_ceiling(self) -> bool {
        self.mean <= CEILING_MEAN && self.p99 <= CEILING_P99 && self.max <= CEILING_MAX
    }
}

/// Why a case failed, or [`Verdict::Pass`].
///
/// An enum rather than a `bool` because the transcript goes on #893 and "which
/// of the ways did it fail" is the whole value of pasting it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Inside the ceiling on every channel, with the beam where the kit put it.
    Pass,
    /// **Every compared pixel is `0, 0, 0`** — the GL arm drew nothing at all.
    ///
    /// The failure shape #1070's review finding M2 named
    /// (<https://github.com/vibec0re/trollshell/pull/1070#issuecomment-5621283282>):
    /// a glvnd stub for an unimplemented entry point returns `0` without
    /// trapping, so a wrong dispatch table degrades to an empty compile log or
    /// a no-op texture allocation — a **silent black chip**, with no GL error,
    /// no `GLArea::error()` and a live context.
    ///
    /// **This is a split of [`Self::UndrawnFramebuffer`], not new coverage, and
    /// the distinction matters to anyone editing [`Stats::verdict_for`].** An
    /// all-zero readback is a strict subset of "one flat colour", and `uniform`
    /// was already checked ahead of the ceiling before #1072, so M2's "a case,
    /// not a pass" was satisfied without this variant — measured: deleting the
    /// `all_zero` branch reports `UndrawnFramebuffer`, not `PASS`. What this
    /// adds is the *diagnosis*, which is worth having, because "the GL arm drew
    /// nothing" and "the GL arm drew a flat colour" send a reader to different
    /// bugs. What it does **not** do is make the `uniform` branch redundant:
    /// remove that one believing this covers its ground and a flat *non-black*
    /// framebuffer starts reporting `PASS`.
    RendersNothing,
    /// The readback is a single flat colour, so nothing was drawn — see
    /// [`Stats::uniform`].
    UndrawnFramebuffer,
    /// At least one channel is outside mean/p99/max.
    OverCeiling,
    /// A column's brightest row moved further than
    /// [`peak_row_tolerance`] — a beam drawn somewhere else, not a rounding
    /// disagreement.
    BeamMoved,
    /// Inside the ceiling, but not **bit-exact**, under
    /// `TROLLSHELL_PARITY_EXACT=1` — and for a [`Kind`] that env pins (#1080).
    ///
    /// Named separately from [`Self::OverCeiling`] so a transcript can tell
    /// "outside the ceiling" from "inside the ceiling, but not the zero this
    /// sandbox is pinned to" at a glance.
    NotBitExact,
    /// A **supersampled** case disagrees somewhere that is not a rasterisation
    /// edge (#1148 review, HIGH-2) — see [`Regions::interior_max`].
    ///
    /// The sharp one of the two supersampled checks, and the reason that
    /// comparison is worth running at all. Everything the two arms are *meant*
    /// to differ about lives on an edge; a flat fill, the interior of a tick, a
    /// lit core or the CRT's comb that moved says the face itself was resolved
    /// differently at this scale, which is what a dropped half-pixel offset, an
    /// unscaled length, a doubled mask pitch or a mis-scaled bloom does.
    InteriorMoved,
    /// A supersampled case's edge region is outside
    /// [`SUPERSAMPLED_EDGE_MEAN`]/[`SUPERSAMPLED_EDGE_MAX`].
    EdgeOverBudget,
}

/// Which kit widget a case is measuring, because the two do **not** take the
/// same structural checks (#1143).
///
/// The ceiling (mean ≤ 2 / p99 ≤ 8 / max ≤ 32 per channel) is #893's, and it
/// is what Annika agreed to on that thread — it is what every real driver
/// faces, and nothing here touches it. What `TROLLSHELL_PARITY_EXACT=1` adds
/// on top is a *measurement*, not a design target: it is exported in exactly
/// one place, `nix/checks/system-tests.nix`, inside the llvmpipe sandbox, so
/// it is a regression detector for one pinned software rasteriser and costs
/// nothing on hardware.
///
/// **Both kinds are pinned there** (#1148 review, HIGH-1). The gauge was not,
/// on the argument that it is a different rasteriser — the kit walks a
/// bounding box per shape accumulating `u16` coverage, the shader evaluates
/// the same distance fields per fragment in `highp float` — and that Annika's
/// "does not have to be pixel perfect identical" on #865 said so. Both halves
/// were wrong about *this* variable. Annika's word was permission for the GL
/// arm to look different on glass, which the ceiling already grants; and the
/// twelve gauge cases have measured `max |Δ| 0` on every channel under
/// llvmpipe since the day the arm landed, so the sensitivity is there to be
/// had. Without the pin the review's 9 % widening of every minor tick
/// (`gauge.frag`'s `MINOR_HW`, 0.55 → 0.60) reported worst-channel mean 0.044
/// / p99 2 / max 3 and shipped green — ~45× inside a ceiling built for a GPU
/// nobody has run this on.
///
/// A pin is only ever as good as the measurement under it, and the Mesa-bump
/// risk is real: `nix flake update` can red this on a PR that touched no
/// shader. That risk was accepted for the scope in #1080 and
/// `nix/checks/system-tests.nix` already documents the response (re-measure on
/// the new Mesa; never raise the ceiling).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// `preem.scope` — pinned bit-exact under `TROLLSHELL_PARITY_EXACT=1`, and
    /// structurally checked with the per-column peak-row test.
    Scope,
    /// `preem.gauge` (#1143) — pinned bit-exact too, since #1148's review; no
    /// peak-row check, which is a beam statistic.
    Gauge,
}

impl Kind {
    /// Whether `TROLLSHELL_PARITY_EXACT=1` holds this kind to a zero delta.
    ///
    /// **Only what has been measured at zero** — which, since #1148's review,
    /// is both of them. See the type docs.
    ///
    /// An exhaustive `match` rather than a `matches!`, deliberately: a third
    /// kind (#1144) must not inherit an answer by falling off the end of a
    /// pattern. Whoever adds it has to look at their own llvmpipe numbers and
    /// say which of the two this is, and the compiler makes them.
    pub(crate) fn pinned_exact(self) -> bool {
        match self {
            Self::Scope | Self::Gauge => true,
        }
    }

    /// Whether the per-column **peak-row** check applies.
    ///
    /// It is a beam test, and it says so in its own name: "a column's
    /// brightest row moved" is a structural statement about a trace that has
    /// exactly one bright row per column. A dial's brightest row in a column is
    /// whichever of the arc, a tick, the needle and the hub happens to win
    /// there, and two of those can tie at the same intensity — so on a gauge
    /// the statistic measures which shape `argmax` saw first, not whether the
    /// picture moved. The ceiling and the blank-framebuffer guards do the work
    /// for a gauge; inventing a second structural check for it without a
    /// failure to calibrate against would be inventing a flake.
    pub(crate) fn checks_peak_rows(self) -> bool {
        matches!(self, Self::Scope)
    }

    /// The word the transcript prints.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Scope => "scope",
            Self::Gauge => "gauge",
        }
    }
}

/// How a case's two buffers were brought to the same grid before comparing
/// (#1148 review, HIGH-2).
///
/// The exact pin is a statement about **one** comparison: the GL arm and the
/// kit rasterising the same picture at the same resolution, pixel against
/// pixel. A supersampled case is a different question — the GL arm rendered at
/// `factor` times the kit's grid and the harness box-averaged it back down —
/// and box-averaging a differently-anti-aliased render is not an operation
/// that can land on the kit's last bit, nor should it: the whole point of
/// rendering at `scale = 2` is that the edges are *not* the kit's. So those
/// cases take the #893 ceiling and stop there, whatever [`Kind::pinned_exact`]
/// says.
///
/// It is a property of the case rather than of the kind for the same reason
/// `Kind` exists at all: one widget can be measured both ways, and the gauge is
/// — `scale = 1` pinned bit-exact, `scale = 2` held to the split
/// [`case_verdict`] describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Sampling {
    /// One GL pixel per kit pixel: the comparison the exact pin is about.
    OneToOne,
    /// The GL arm rendered `factor`× the kit's grid on each axis and the
    /// readback was box-averaged back down — see [`box_downsample`].
    Supersampled(u32),
}

/// The full verdict for one case: the guards, then whichever standard this
/// *comparison* is held to.
///
/// Split from [`Stats::verdict_for`] rather than folded into it because the two
/// answer different questions: that one is "do these two buffers agree", a
/// property of the pixels alone, and this one is "does this *case* pass",
/// which also depends on which widget it is, how it was sampled, and on an
/// environment variable. Keeping the pixel statistic free of all three is what
/// lets the tests below drive it on synthetic buffers.
///
/// **Two standards, because there are two comparisons** (#1148 review, HIGH-2):
///
/// * [`Sampling::OneToOne`] — #893's per-channel ceiling, the scope's peak-row
///   check, and the `TROLLSHELL_PARITY_EXACT=1` zero pin on both kinds. Two
///   renderers drawing the same picture at the same resolution: a disagreement
///   is rounding, and under llvmpipe there has not been one.
/// * [`Sampling::Supersampled`] — **every pixel off a rasterisation edge is
///   bit-identical**, and the edge region is inside its own budget. The GL arm
///   drew at twice the density and the harness averaged it back down, so the
///   edges are *supposed* to differ — that is #1090's fix — while nothing else
///   is. Measured on llvmpipe, all four shipping-scale cases come out with the
///   field and the lit interiors at `max |Δ| 0` and everything in the edge bin.
///   #893's ceiling is deliberately **not** applied here: it is a statement
///   about rounding between two renders of one picture, and half the frame's
///   pixels are edges on a dial.
pub(crate) fn case_verdict(
    stats: &Stats,
    regions: &Regions,
    kind: Kind,
    sampling: Sampling,
    exact: bool,
) -> Verdict {
    // "Drew nothing at all", then "drew one flat colour" — ahead of everything,
    // on every comparison, for #1070's M2 reason.
    if stats.all_zero {
        return Verdict::RendersNothing;
    }
    if stats.uniform {
        return Verdict::UndrawnFramebuffer;
    }
    match sampling {
        Sampling::Supersampled(_) => {
            if regions.interior_max() > 0 {
                return Verdict::InteriorMoved;
            }
            if regions.edge.mean > SUPERSAMPLED_EDGE_MEAN
                || regions.edge.max > SUPERSAMPLED_EDGE_MAX
            {
                return Verdict::EdgeOverBudget;
            }
            Verdict::Pass
        }
        Sampling::OneToOne => {
            let verdict = stats.verdict_for(kind);
            if !verdict.is_pass() {
                return verdict;
            }
            // `max == 0.0` on every channel is equivalent to "every compared
            // pixel had `|Δ| == 0`": mean and p99 are drawn from that same
            // non-negative distribution and cannot exceed its max.
            let bit_exact = stats.channels.iter().all(|c| c.max == 0.0);
            if exact && kind.pinned_exact() && !bit_exact {
                return Verdict::NotBitExact;
            }
            Verdict::Pass
        }
    }
}

/// One region's |Δ| distribution — see [`regions`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct RegionStats {
    /// How many reference pixels fell in this bin.
    pub(crate) pixels: usize,
    /// Mean worst-channel |Δ| over them, in 255ths.
    pub(crate) mean: f64,
    /// The worst single pixel in the bin.
    pub(crate) max: u8,
}

/// The **edge / field / lit** split of one case's deltas.
///
/// The classification aid #1072 added, and the reason a transcript from this
/// harness can be triaged without a driver in front of you. Every compared
/// pixel goes in exactly one bin, decided by the **CPU reference's own
/// structure** rather than by the palette — so this needs no knowledge of which
/// skin is running and cannot drift from one:
///
/// * **edge** — the reference disagrees with one of its four neighbours. A
///   difference that lives only here is rasterisation edge coverage: a boundary
///   landing one pixel over.
/// * **field** — not an edge, and the frame's *modal* colour, i.e. the flat
///   unlit background. A difference here is a tone-curve problem: gamma/sRGB
///   moves a flat fill uniformly, and nothing else does.
/// * **lit** — not an edge, not the field: the interior of the trace, the
///   graticule and the bloom. A difference concentrated here, with the field
///   clean, is real shader math — or, where it tracks how *dim* the pixel is,
///   blend/premultiply.
///
/// It lived in the example until #1148's review, printed and never read. It is
/// here now because the supersampled verdict is a *statement about the split*
/// (everything off an edge must be bit-identical), and because down here it has
/// tests.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Regions {
    /// Pixels the reference itself draws a boundary through.
    pub(crate) edge: RegionStats,
    /// The flat modal colour, off any edge.
    pub(crate) field: RegionStats,
    /// Everything else off an edge: interiors, bloom, the lit core.
    pub(crate) lit: RegionStats,
    /// The modal colour the `field` bin was decided by, for the transcript.
    pub(crate) field_colour: [u8; 3],
}

impl Regions {
    /// The worst |Δ| over every pixel that is **not** on a rasterisation edge.
    ///
    /// The supersampled case's whole assertion, in one number. See
    /// [`Verdict::InteriorMoved`].
    pub(crate) fn interior_max(self) -> u8 {
        self.field.max.max(self.lit.max)
    }
}

/// Bin one case's per-pixel deltas by the reference's own structure.
///
/// `reference` is top-down RGBA8 at `size`; `deltas` is [`delta_map`]'s output,
/// one worst-channel |Δ| byte per pixel in the same order. A short `reference`
/// reads as black and a short `deltas` as zero, the same
/// no-panic-in-the-harness rule the rest of this module follows.
pub(crate) fn regions(reference: &[u8], size: (usize, usize), deltas: &[u8]) -> Regions {
    let (w, h) = size;
    let rgb = |x: usize, y: usize| -> [u8; 3] {
        let i = (y * w + x) * 4;
        reference
            .get(i..i + 3)
            .map_or([0, 0, 0], |px| [px[0], px[1], px[2]])
    };
    let mut counts: std::collections::HashMap<[u8; 3], usize> = std::collections::HashMap::new();
    for y in 0..h {
        for x in 0..w {
            *counts.entry(rgb(x, y)).or_default() += 1;
        }
    }
    let field_colour = counts
        .into_iter()
        .max_by_key(|(colour, n)| (*n, *colour))
        .map_or([0, 0, 0], |(colour, _)| colour);

    // (count, sum, max) per bin, in edge / field / lit order.
    let mut bins = [(0_usize, 0_u64, 0_u8); 3];
    for y in 0..h {
        for x in 0..w {
            let here = rgb(x, y);
            let edge = [(-1_isize, 0_isize), (1, 0), (0, -1), (0, 1)]
                .into_iter()
                .any(|(dx, dy)| {
                    let (nx, ny) = (x.wrapping_add_signed(dx), y.wrapping_add_signed(dy));
                    nx < w && ny < h && rgb(nx, ny) != here
                });
            let bin = if edge {
                0
            } else if here == field_colour {
                1
            } else {
                2
            };
            let delta = deltas.get(y * w + x).copied().unwrap_or(0);
            bins[bin].0 += 1;
            bins[bin].1 += u64::from(delta);
            bins[bin].2 = bins[bin].2.max(delta);
        }
    }
    let stats = |(pixels, sum, max): (usize, u64, u8)| {
        #[allow(clippy::cast_precision_loss)]
        let mean = if pixels == 0 {
            0.0
        } else {
            sum as f64 / pixels as f64
        };
        RegionStats { pixels, mean, max }
    };
    Regions {
        edge: stats(bins[0]),
        field: stats(bins[1]),
        lit: stats(bins[2]),
        field_colour,
    }
}

/// Box-average a bottom-up RGBA8 framebuffer readback down by `factor` on each
/// axis, for the supersampled comparison [`Sampling::Supersampled`] names.
///
/// Returns the averaged buffer and its new `(width, height)`, in the **same**
/// bottom-up convention, so the result drops straight into a [`Layout`] with no
/// other change. Row order is a reversal and blocks of `factor` rows stay
/// blocks under it as long as the height divides, which it does: the GL grid is
/// the kit's grid times the integer upscale.
///
/// Rounding is half-up over the block (`+ n/2` before the divide), which is the
/// kit's own convention in `mix` — the harness must not introduce a bias of its
/// own to the thing it is measuring.
///
/// A `factor` of `0` or `1`, or an allocation that does not divide by it, hands
/// the buffer back untouched: this is a harness, and silently reshaping a
/// framebuffer it did not understand is exactly how #1072 measured nothing for
/// twelve cases.
pub(crate) fn box_downsample(gl: &[u8], alloc: (u32, u32), factor: u32) -> (Vec<u8>, (u32, u32)) {
    let (w, h) = (alloc.0 as usize, alloc.1 as usize);
    let n = factor as usize;
    if n <= 1 || w % n != 0 || h % n != 0 || gl.len() < w * h * 4 {
        return (gl.to_vec(), alloc);
    }
    let (out_w, out_h) = (w / n, h / n);
    // `factor` came in as a `u32` and `n * n` is at most `u32::MAX²` only for a
    // factor no allocation could divide by; the guard above has already refused
    // anything that does not divide `w` and `h`, so this is a small number.
    let taps = u32::try_from(n * n).unwrap_or(u32::MAX);
    let mut out = Vec::with_capacity(out_w * out_h * 4);
    for by in 0..out_h {
        for bx in 0..out_w {
            for channel in 0..4 {
                let mut sum = 0_u32;
                for dy in 0..n {
                    for dx in 0..n {
                        let i = (((by * n + dy) * w) + bx * n + dx) * 4 + channel;
                        sum += u32::from(gl[i]);
                    }
                }
                #[allow(clippy::cast_possible_truncation)]
                out.push(((sum + taps / 2) / taps).min(255) as u8);
            }
        }
    }
    (
        out,
        (
            u32::try_from(out_w).unwrap_or(0),
            u32::try_from(out_h).unwrap_or(0),
        ),
    )
}

impl Verdict {
    /// The word the transcript prints.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::RendersNothing => "FAIL(nothing)",
            Self::UndrawnFramebuffer => "FAIL(blank)",
            Self::OverCeiling => "FAIL(ceiling)",
            Self::BeamMoved => "FAIL(beam)",
            Self::NotBitExact => "FAIL(exact)",
            Self::InteriorMoved => "FAIL(interior)",
            Self::EdgeOverBudget => "FAIL(edges)",
        }
    }

    /// Whether this is the only verdict that is not a failure.
    pub(crate) fn is_pass(self) -> bool {
        self == Self::Pass
    }
}

/// One comparison's full result.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Stats {
    /// R, G and B, each with its own distribution.
    pub(crate) channels: [ChannelStats; 3],
    /// How many **pixels** were compared — not bytes, and not
    /// channel-samples. A 96×48 frame is 4608 of these.
    pub(crate) pixels: usize,
    /// Columns whose brightest row moved further than [`peak_row_tolerance`].
    /// Zero is the only acceptable answer, and it is **in** the verdict: a
    /// trace drawn in the wrong place with the right colours can sit well
    /// inside a per-channel mean.
    pub(crate) peak_row_mismatches: u32,
    /// Every pixel of the GL readback carries the same colour.
    ///
    /// The guard against measuring an **undrawn** framebuffer, which is the
    /// harness's most dangerous failure because every other check passes
    /// through it: `GlSurface::draw` returns silently when the program is
    /// unregistered, the state is unset or a shader failed to build, and in
    /// all three `GLArea::error()` is `None`, `context()` is `Some` and
    /// `glGetError` is clean. A `Scope` always paints a graticule, so a real
    /// frame is never uniform.
    pub(crate) uniform: bool,
    /// Every compared GL pixel is `0, 0, 0` — see [`Verdict::RendersNothing`].
    ///
    /// A strict subset of [`Self::uniform`], reported separately because it is
    /// the one failure shape a reader has to be told by name: "the GL arm drew
    /// nothing" is a different bug report from "the GL arm drew a flat
    /// colour", and #1070's M2 is specifically the first.
    pub(crate) all_zero: bool,
    /// The single worst compared pixel, for the transcript: where it is, which
    /// channel, and what each arm put there.
    ///
    /// `None` only for an empty comparison. Reported because a bare `max 141`
    /// classifies nothing — the *coordinates* are what say whether a
    /// disagreement sits on an edge (rasterisation), in the interior of a flat
    /// fill (gamma, or shader math), or only where the trace is (blend).
    pub(crate) worst: Option<WorstPixel>,
}

/// The worst compared pixel of one case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WorstPixel {
    /// Reference-frame column.
    pub(crate) x: usize,
    /// Reference-frame row, top-down (the kit's convention).
    pub(crate) y: usize,
    /// Index into [`CHANNELS`].
    pub(crate) channel: usize,
    /// `|Δ|` on that channel, in 255ths.
    pub(crate) delta: u8,
    /// What the GL arm put there, RGB.
    pub(crate) gl: [u8; 3],
    /// What the CPU kit put there, RGB.
    pub(crate) cpu: [u8; 3],
}

impl Stats {
    /// The worst channel's mean.
    pub(crate) fn worst_mean(&self) -> f64 {
        self.channels.iter().map(|c| c.mean).fold(0.0, f64::max)
    }

    /// The worst channel's p99.
    pub(crate) fn worst_p99(&self) -> f64 {
        self.channels.iter().map(|c| c.p99).fold(0.0, f64::max)
    }

    /// The worst channel's max.
    pub(crate) fn worst_max(&self) -> f64 {
        self.channels.iter().map(|c| c.max).fold(0.0, f64::max)
    }

    /// The pixel verdict for one kind, in the order a reader wants to be told:
    /// a blank framebuffer first (because it makes every other number
    /// meaningless), then the ceiling, then the structural check — which is
    /// the scope's alone (see [`Kind::checks_peak_rows`]).
    ///
    /// It takes a [`Kind`] and **not** the `TROLLSHELL_PARITY_EXACT` flag: this
    /// is a property of the two buffers, which is what lets the tests below
    /// drive it on synthetic ones. [`case_verdict`] is the whole answer.
    pub(crate) fn verdict_for(&self, kind: Kind) -> Verdict {
        // "Drew nothing at all" first, then "drew one flat colour": the second
        // is the general case of the first, and the first is the one #1070's
        // M2 says must never be reported as anything else.
        if self.all_zero {
            return Verdict::RendersNothing;
        }
        if self.uniform {
            return Verdict::UndrawnFramebuffer;
        }
        if !self
            .channels
            .iter()
            .copied()
            .all(ChannelStats::inside_ceiling)
        {
            return Verdict::OverCeiling;
        }
        if kind.checks_peak_rows() && self.peak_row_mismatches > 0 {
            return Verdict::BeamMoved;
        }
        Verdict::Pass
    }
}

/// Where the GL readback and the CPU reference each live, and how to walk them.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Layout {
    /// The GL framebuffer's `(width, height)` in device pixels.
    pub(crate) alloc: (u32, u32),
    /// The CPU reference frame's `(width, height)`.
    pub(crate) reference: (usize, usize),
    /// The framebuffer's integer device-scale multiplier.
    pub(crate) device_scale: u32,
    /// How many **reference-frame pixels** a column's brightest row may move
    /// before it counts as a structural difference — see
    /// [`peak_row_tolerance`], which is what [`Layout::for_capture`] fills this
    /// with and the only thing that should.
    pub(crate) peak_row_tolerance: usize,
}

impl Layout {
    /// The layout for one capture: a `GtkGLArea` readback at `alloc` device
    /// pixels, against a kit frame of `reference` pixels rendered at `upscale`.
    ///
    /// A constructor rather than a struct literal so [`peak_row_tolerance`] is
    /// the *only* way the beam tolerance gets set. It was a hand-written
    /// expression at the call site before, and it drifted: it carried a stray
    /// device-scale factor (making the verdict depend on the reviewer's
    /// monitor) while two comments described a different number again. The
    /// tests below drive this function, not a literal, which is what would have
    /// caught that.
    pub(crate) fn for_capture(
        alloc: (u32, u32),
        reference: (usize, usize),
        device_scale: u32,
        upscale: usize,
    ) -> Self {
        Self {
            alloc,
            reference,
            device_scale,
            peak_row_tolerance: peak_row_tolerance(upscale),
        }
    }
}

/// Compare a GL framebuffer readback against the kit's frame.
///
/// `gl` is bottom-up RGBA8 at `layout.alloc`; `reference` is top-down RGBA8 at
/// `layout.reference`. Both are plain slices and nothing here touches GL, which
/// is what lets the tests below drive it on synthetic buffers.
///
/// A short or absent `gl` slice reads as black rather than panicking, and then
/// shows up as [`Stats::uniform`] — the guard, not a silent zero.
pub(crate) fn compare(gl: &[u8], reference: &[u8], layout: Layout) -> Stats {
    let (ref_w, ref_h) = layout.reference;

    let mut deltas: [Vec<u8>; 3] = std::array::from_fn(|_| Vec::with_capacity(ref_w * ref_h));
    let mut peak_row_mismatches = 0_u32;
    let mut first_gl: Option<[u8; 3]> = None;
    let mut uniform = true;
    let mut all_zero = true;
    let mut worst: Option<WorstPixel> = None;

    let luma = |px: [u8; 3]| u32::from(px[0]) + u32::from(px[1]) + u32::from(px[2]);

    for x in 0..ref_w {
        let (mut gl_peak, mut gl_peak_row) = (0, 0);
        let (mut cpu_peak, mut cpu_peak_row) = (0, 0);
        for y in 0..ref_h {
            let (g, c) = (
                gl_pixel(gl, layout, x, y),
                ref_pixel(reference, layout, x, y),
            );
            match first_gl {
                None => first_gl = Some(g),
                Some(seen) if seen != g => uniform = false,
                Some(_) => {}
            }
            all_zero &= g == [0, 0, 0];
            for (channel, bucket) in deltas.iter_mut().enumerate() {
                let delta = g[channel].abs_diff(c[channel]);
                bucket.push(delta);
                // `>` and not `>=`, so the worst pixel reported is the *first*
                // one at that magnitude in this loop's own **column-major**
                // order (x outer, then y, then R/G/B) — a stable coordinate to
                // paste into a transcript rather than whichever tie the loop
                // happened to end on.
                //
                // Column-major is structural, not a choice: the per-column peak
                // rows above accumulate across the inner loop, so `x` has to be
                // the outer one. The comment here said "row-major" until #1072's
                // review measured otherwise; the test below now pins the order
                // it actually walks, because that order *is* the coordinate a
                // transcript gets triaged from.
                if worst.is_none_or(|held| delta > held.delta) {
                    worst = Some(WorstPixel {
                        x,
                        y,
                        channel,
                        delta,
                        gl: g,
                        cpu: c,
                    });
                }
            }
            if luma(g) > gl_peak {
                gl_peak = luma(g);
                gl_peak_row = y;
            }
            if luma(c) > cpu_peak {
                cpu_peak = luma(c);
                cpu_peak_row = y;
            }
        }
        if gl_peak_row.abs_diff(cpu_peak_row) > layout.peak_row_tolerance {
            peak_row_mismatches += 1;
        }
    }

    let empty = deltas[0].is_empty();
    Stats {
        channels: std::array::from_fn(|channel| distribution(&mut deltas[channel])),
        pixels: ref_w * ref_h,
        peak_row_mismatches,
        // An empty comparison is not "uniform", it is nothing at all — but it
        // is still not a frame, so it fails the same way.
        uniform: uniform || empty,
        // A readback that never arrived reads as zeros through `gl_pixel`,
        // which is the same report as one that arrived full of them: in both
        // the GL arm put no pixels in front of the comparison.
        all_zero,
        worst,
    }
}

/// One pixel of the GL readback, in **reference-frame** coordinates.
///
/// Bottom-up (GL's window origin), point-sampled through the device scale, and
/// out-of-range reads as black rather than panicking — which is what surfaces
/// a short or absent readback as [`Stats::all_zero`] instead of a silent zero.
fn gl_pixel(gl: &[u8], layout: Layout, x: usize, y: usize) -> [u8; 3] {
    let (alloc_w, alloc_h) = (layout.alloc.0 as usize, layout.alloc.1 as usize);
    let scale = (layout.device_scale as usize).max(1);
    let (sx, sy) = (x * scale, y * scale);
    let flipped = alloc_h.saturating_sub(1).saturating_sub(sy);
    let i = (flipped * alloc_w + sx) * 4;
    gl.get(i..i + 3)
        .map_or([0, 0, 0], |px| [px[0], px[1], px[2]])
}

/// One pixel of the CPU reference frame, top-down RGBA8 at `layout.reference`.
fn ref_pixel(reference: &[u8], layout: Layout, x: usize, y: usize) -> [u8; 3] {
    let (ref_w, _) = layout.reference;
    let i = (y * ref_w + x) * 4;
    reference
        .get(i..i + 3)
        .map_or([0, 0, 0], |px| [px[0], px[1], px[2]])
}

/// The per-pixel **worst-channel** `|Δ|` over the compared region, row-major in
/// reference-frame order (top-down), one byte per pixel.
///
/// The evidence half of the harness, and the reason #1072 could be classified
/// at all: a mean and a max say *how big* the disagreement is, and a picture of
/// where it sits says *what kind* it is. Concentrated on the trace → blend;
/// flat across a solid fill → gamma or shader math; a one-pixel outline →
/// rasterisation edge coverage. Written out as a PPM by the harness; kept here
/// rather than in the example so it walks the buffers through the same
/// [`gl_pixel`] / [`ref_pixel`] pair the verdict does, and so it has a test.
pub(crate) fn delta_map(gl: &[u8], reference: &[u8], layout: Layout) -> Vec<u8> {
    let (ref_w, ref_h) = layout.reference;
    let mut out = Vec::with_capacity(ref_w * ref_h);
    for y in 0..ref_h {
        for x in 0..ref_w {
            let (g, c) = (
                gl_pixel(gl, layout, x, y),
                ref_pixel(reference, layout, x, y),
            );
            out.push(
                (0..3)
                    .map(|channel| g[channel].abs_diff(c[channel]))
                    .max()
                    .unwrap_or(0),
            );
        }
    }
    out
}

/// The GL readback re-laid-out as a top-down RGB image at the reference size —
/// the "what the GL arm actually drew" half of the evidence pair.
pub(crate) fn gl_image(gl: &[u8], layout: Layout) -> Vec<u8> {
    let (ref_w, ref_h) = layout.reference;
    let mut out = Vec::with_capacity(ref_w * ref_h * 3);
    for y in 0..ref_h {
        for x in 0..ref_w {
            out.extend_from_slice(&gl_pixel(gl, layout, x, y));
        }
    }
    out
}

/// Mean / p99 / max over one channel's deltas. Sorts in place.
fn distribution(deltas: &mut [u8]) -> ChannelStats {
    if deltas.is_empty() {
        return ChannelStats::default();
    }
    let sum: u64 = deltas.iter().map(|d| u64::from(*d)).sum();
    #[allow(clippy::cast_precision_loss)]
    let mean = sum as f64 / deltas.len() as f64;
    deltas.sort_unstable();
    // The index of the 99th percentile, clamped into the slice: `len * 99 /
    // 100` is `len` itself only for `len == 0`, which returned above.
    let p99 = deltas
        .get(deltas.len().saturating_mul(99) / 100)
        .or_else(|| deltas.last())
        .map_or(0.0, |d| f64::from(*d));
    let max = deltas.last().map_or(0.0, |d| f64::from(*d));
    ChannelStats { mean, p99, max }
}

#[cfg(test)]
mod tests {
    use super::{
        CEILING_MAX, CEILING_MEAN, CEILING_P99, ChannelStats, Kind, Layout, RegionStats, Regions,
        SUPERSAMPLED_EDGE_MEAN, Sampling, Stats, Verdict, box_downsample, case_verdict, compare,
        distribution, peak_row_tolerance, regions,
    };

    /// A `Stats` whose **worst pixel** is `delta` 255ths off on every channel,
    /// with nothing else wrong — the shape a real near-miss has, where a
    /// handful of anti-aliased edge pixels disagree and the rest of the frame
    /// is clean.
    fn inside_by(delta: f64) -> Stats {
        let channel = ChannelStats {
            // A hundredth of the worst pixel: a frame where one pixel in a
            // hundred is off by `delta`, which is well inside the mean ceiling
            // for every `delta` this test uses, and exactly `0` at `0`.
            mean: delta / 100.0,
            p99: delta.min(CEILING_P99),
            max: delta,
        };
        Stats {
            channels: [channel, channel, channel],
            pixels: 1,
            peak_row_mismatches: 0,
            uniform: false,
            all_zero: false,
            worst: None,
        }
    }

    /// A clean [`Regions`] — nothing disagrees anywhere — as the supersampled
    /// verdict's "no finding" baseline.
    fn clean_regions() -> Regions {
        Regions {
            edge: RegionStats {
                pixels: 2400,
                mean: 0.0,
                max: 0,
            },
            field: RegionStats {
                pixels: 6700,
                mean: 0.0,
                max: 0,
            },
            lit: RegionStats {
                pixels: 46,
                mean: 0.0,
                max: 0,
            },
            field_colour: [4, 10, 14],
        }
    }

    /// **`TROLLSHELL_PARITY_EXACT=1` pins both kinds at a 1:1 grid** (#1148
    /// review, HIGH-1).
    ///
    /// The pin used to be the scope's alone, on the reading that Annika's "does
    /// not have to be pixel perfect identical" (#865) applied to it. It did
    /// not: that variable is exported in one place, the llvmpipe sandbox
    /// (`nix/checks/system-tests.nix`), so it never reaches the glass her word
    /// was about, and the gauge's twelve `scale = 1` cases have measured
    /// `max |Δ| 0` from the day the arm landed. Unpinned, a 9 % widening of
    /// every minor tick reported a worst-channel max of 3 against a ceiling of
    /// 32 and shipped green.
    ///
    /// **Falsified** three ways, each moving exactly one assertion: make
    /// [`Kind::pinned_exact`] answer `false` for either kind (the first two),
    /// or drop the `exact &&` guard from [`case_verdict`] (the third).
    #[test]
    fn the_exact_pin_binds_both_kinds_at_one_to_one() {
        for kind in [Kind::Scope, Kind::Gauge] {
            assert_eq!(
                case_verdict(
                    &inside_by(1.0),
                    &clean_regions(),
                    kind,
                    Sampling::OneToOne,
                    true,
                ),
                Verdict::NotBitExact,
                "a {} case one 255th off is inside the ceiling and still fails, \
                 because llvmpipe has never measured anything but 0",
                kind.label(),
            );
            assert_eq!(
                case_verdict(
                    &inside_by(0.0),
                    &clean_regions(),
                    kind,
                    Sampling::OneToOne,
                    true,
                ),
                Verdict::Pass,
                "a bit-exact {} case passes the pin",
                kind.label(),
            );
        }
        assert_eq!(
            case_verdict(
                &inside_by(1.0),
                &clean_regions(),
                Kind::Scope,
                Sampling::OneToOne,
                false,
            ),
            Verdict::Pass,
            "…and without the env it is the ceiling alone, on both kinds",
        );
    }

    /// **A supersampled case is judged on the region split, not on #893's
    /// ceiling** (#1148 review, HIGH-2): everything off a rasterisation edge
    /// must be bit-identical, and the edges get their own budget.
    ///
    /// The ceiling is a statement about rounding between two renders of one
    /// picture at one resolution. A `scale = 2` case is not that — the GL arm
    /// drew at twice the density and the harness averaged it back down, so on
    /// every pixel the kit anti-aliased the two have genuinely different
    /// coverage, which *is* #1090's fix. Measured on llvmpipe, all four
    /// shipping-scale cases land with the field and the lit interiors at
    /// `max |Δ| 0` and an edge mean of 6.1 to 9.4 — over #893's mean of 2 on
    /// two of the four skins, which is why holding them to it would have meant
    /// holding the improvement to "do not improve".
    ///
    /// **Falsified** three ways: drop the `interior_max` check (the first
    /// assertion passes a moved flat fill), drop the edge budget (the second),
    /// or apply `verdict_for`'s ceiling to the supersampled arm (the last, and
    /// it is the one that says why the split exists at all).
    #[test]
    fn a_supersampled_case_is_exact_off_the_edges_and_budgeted_on_them() {
        let edgy = Stats {
            channels: [ChannelStats {
                mean: 8.0,
                p99: 40.0,
                max: 76.0,
            }; 3],
            ..inside_by(0.0)
        };
        let mut moved_field = clean_regions();
        moved_field.field.max = 1;
        moved_field.field.mean = 0.01;
        assert_eq!(
            case_verdict(
                &edgy,
                &moved_field,
                Kind::Gauge,
                Sampling::Supersampled(2),
                true,
            ),
            Verdict::InteriorMoved,
            "one 255th on a flat fill is the face resolved differently at this scale, \
             and that is the check with the teeth",
        );

        let mut wild_edges = clean_regions();
        wild_edges.edge.mean = SUPERSAMPLED_EDGE_MEAN + 0.5;
        assert_eq!(
            case_verdict(
                &edgy,
                &wild_edges,
                Kind::Gauge,
                Sampling::Supersampled(2),
                true,
            ),
            Verdict::EdgeOverBudget,
            "the edges still have a budget — a face drawn somewhere else is all edge",
        );

        let mut measured = clean_regions();
        measured.edge.mean = 9.364;
        measured.edge.max = 76;
        assert_eq!(
            case_verdict(
                &edgy,
                &measured,
                Kind::Gauge,
                Sampling::Supersampled(2),
                true,
            ),
            Verdict::Pass,
            "…and the numbers llvmpipe actually measures pass, over #893's mean of 2 \
             on the worst-channel statistic and clean everywhere but the edges",
        );
    }

    /// Every guard fires **ahead** of both standards: a breach is reported as a
    /// breach, not as "not bit-exact" or as an edge budget, whichever
    /// comparison it came from.
    #[test]
    fn the_ceiling_and_the_blank_guards_come_before_the_pin() {
        let over = inside_by(CEILING_MAX + 1.0);
        for kind in [Kind::Scope, Kind::Gauge] {
            assert_eq!(
                case_verdict(&over, &clean_regions(), kind, Sampling::OneToOne, true),
                Verdict::OverCeiling,
                "{} at 1:1 is still held to #893's ceiling",
                kind.label(),
            );
        }
        let blank = Stats {
            uniform: true,
            all_zero: true,
            ..inside_by(0.0)
        };
        for sampling in [Sampling::OneToOne, Sampling::Supersampled(2)] {
            assert_eq!(
                case_verdict(&blank, &clean_regions(), Kind::Gauge, sampling, true),
                Verdict::RendersNothing,
                "a gauge that drew nothing is bit-exactly nothing — the guard, not the \
                 standard behind it",
            );
        }
    }

    /// The **peak-row** check is the scope's alone (#1143): it is a statement
    /// about a trace with one bright row per column, and a dial's brightest row
    /// in a column is whichever of the arc, a tick, the needle and the hub wins
    /// there — two of which can tie, making the statistic a report on which
    /// shape `argmax` saw first.
    ///
    /// **Falsified** by making [`Kind::checks_peak_rows`] answer `true` for the
    /// gauge: the first assertion reports `FAIL(beam)`.
    #[test]
    fn the_peak_row_check_is_the_beams_and_the_gauge_does_not_take_it() {
        let moved = Stats {
            peak_row_mismatches: 3,
            ..inside_by(0.0)
        };
        assert_eq!(
            case_verdict(
                &moved,
                &clean_regions(),
                Kind::Gauge,
                Sampling::OneToOne,
                true,
            ),
            Verdict::Pass,
            "a gauge is not a beam",
        );
        assert_eq!(
            case_verdict(
                &moved,
                &clean_regions(),
                Kind::Scope,
                Sampling::OneToOne,
                true,
            ),
            Verdict::BeamMoved,
            "…and the scope keeps the check unchanged",
        );
    }

    /// [`regions`] bins by the **reference's** structure: a pixel next to a
    /// different colour is an edge, the modal colour off an edge is the field,
    /// and everything else off an edge is lit.
    ///
    /// The supersampled verdict is a statement about that split, so the split
    /// has to be right. It lived in the example and was printed but never
    /// asserted until #1148's review gave it a verdict to feed.
    ///
    /// **Falsified** by dropping the neighbour test (every pixel lands in field
    /// or lit and `interior_max` reports the edge's own delta, which is the one
    /// way the supersampled check could silently pass everything).
    #[test]
    fn the_region_split_bins_by_the_references_own_structure() {
        // A 4×3 reference: left half field, right half a lit block.
        let (w, h) = (4_usize, 3_usize);
        let mut reference = Vec::new();
        for _ in 0..h {
            for x in 0..w {
                let px: [u8; 4] = if x < 2 {
                    [10, 10, 10, 255]
                } else {
                    [90, 90, 90, 255]
                };
                reference.extend_from_slice(&px);
            }
        }
        // One delta on the boundary column, one in the middle of the field.
        let mut deltas = vec![0_u8; w * h];
        deltas[1] = 7; // (1, 0) — next to the colour change, so an edge
        deltas[w * 2] = 0; // (0, 2) — field, clean
        let split = regions(&reference, (w, h), &deltas);
        assert_eq!(
            split.field_colour,
            [90, 90, 90],
            "half and half: the modal colour is a tie, broken by the larger value",
        );
        assert_eq!(
            split.edge.pixels, 6,
            "the two columns either side of the colour change, and only those",
        );
        assert_eq!(
            split.edge.max, 7,
            "the boundary delta lands in the edge bin"
        );
        assert_eq!(split.interior_max(), 0, "and nothing off an edge disagrees");

        // Widen the frame so there are interior pixels — and make one colour
        // clearly modal, so the field is the flat fill rather than a coin toss.
        let (w, h) = (8_usize, 3_usize);
        let mut reference = Vec::new();
        for _ in 0..h {
            for x in 0..w {
                let px: [u8; 4] = if x < 6 {
                    [10, 10, 10, 255]
                } else {
                    [90, 90, 90, 255]
                };
                reference.extend_from_slice(&px);
            }
        }
        let mut deltas = vec![0_u8; w * h];
        deltas[w + 1] = 5; // (1, 1) — two columns from the boundary: field
        let split = regions(&reference, (w, h), &deltas);
        assert_eq!(
            split.field.max, 5,
            "a flat fill that moved is a field finding"
        );
        assert_eq!(
            split.interior_max(),
            5,
            "…and that is what the verdict reads"
        );
        assert_eq!(split.edge.max, 0, "the edges are clean here");
    }

    /// [`box_downsample`] averages each `factor`×`factor` block, keeps the
    /// bottom-up row order, and hands the buffer back untouched when it cannot
    /// do either.
    ///
    /// The three properties the `scale = 2` harness cases rest on. The middle
    /// one is the one that would fail silently: a downsample that reversed the
    /// rows would still produce a plausible-looking frame, and every delta
    /// would then be measured against a vertically mirrored dial — which on a
    /// gauge, whose face is roughly top-heavy but not symmetric, reads as a
    /// large-but-not-absurd number rather than as an obvious bug.
    ///
    /// **Falsified** by averaging `n` columns but only one row (the gradient
    /// assertion goes red), by iterating the output rows in reverse (the same
    /// one), or by dropping the `w % n != 0` guard (the last assertion panics
    /// on an out-of-range index instead of returning).
    #[test]
    fn the_box_downsample_averages_blocks_and_keeps_the_row_order() {
        // 4×4 RGBA, bottom-up, with a distinct value per row so a flip shows.
        let mut buf = Vec::new();
        for y in 0..4_u8 {
            for _ in 0..4 {
                buf.extend_from_slice(&[y * 10, y * 10 + 1, y * 10 + 2, 255]);
            }
        }
        let (out, alloc) = box_downsample(&buf, (4, 4), 2);
        assert_eq!(alloc, (2, 2), "each axis halves");
        assert_eq!(out.len(), 2 * 2 * 4, "…and so does the buffer");
        // Rows 0 and 1 average to 5 on R; rows 2 and 3 to 25. Bottom-up order
        // means the first output row is still the first input block.
        assert_eq!(
            out[0], 5,
            "the first block is the average of input rows 0-1"
        );
        assert_eq!(
            out[2 * 4],
            25,
            "and the second output row is input rows 2-3, not 0-1 mirrored",
        );
        assert_eq!(out[3], 255, "alpha rides through the same average");

        let (same, alloc) = box_downsample(&buf, (4, 4), 1);
        assert_eq!(
            (same.len(), alloc),
            (buf.len(), (4, 4)),
            "factor 1 is a copy"
        );
        let (odd, alloc) = box_downsample(&buf, (4, 4), 3);
        assert_eq!(
            (odd.len(), alloc),
            (buf.len(), (4, 4)),
            "a factor the allocation does not divide by is refused, not guessed",
        );
    }

    /// Box-averaging a frame that is already the replication of a smaller one
    /// recovers the smaller one **exactly**.
    ///
    /// This is the arithmetic the `scale = 2` cases lean on: the kit renders
    /// logically and replicates, so its own native frame downsamples back to
    /// its logical frame with no residue, and every 255th the harness then
    /// reports is the GL arm's own. Half-up rounding is what makes it exact
    /// here — every block is four copies of one value, so the sum is `4v` and
    /// `(4v + 2) / 4 == v` for every `v` in `0..=255`.
    #[test]
    fn a_replicated_frame_downsamples_back_to_itself() {
        let logical: Vec<u8> = (0..(3_u32 * 2 * 4))
            .map(|i| u8::try_from(i * 7 % 256).unwrap_or(0))
            .collect();
        let (w, h) = (3_usize, 2_usize);
        let mut native = vec![0_u8; w * 2 * h * 2 * 4];
        for y in 0..h * 2 {
            for x in 0..w * 2 {
                let src = ((y / 2) * w + x / 2) * 4;
                let dst = (y * w * 2 + x) * 4;
                native[dst..dst + 4].copy_from_slice(&logical[src..src + 4]);
            }
        }
        let (back, alloc) = box_downsample(&native, (6, 4), 2);
        assert_eq!(alloc, (3, 2));
        assert_eq!(
            back, logical,
            "a 2× replication box-averages back to itself"
        );
    }

    /// A `w`×`h` top-down RGBA8 frame from a per-pixel colour function.
    fn frame(w: usize, h: usize, colour: impl Fn(usize, usize) -> [u8; 3]) -> Vec<u8> {
        let mut out = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                let c = colour(x, y);
                out.extend_from_slice(&[c[0], c[1], c[2], 0xff]);
            }
        }
        out
    }

    /// The same frame in GL's bottom-up order, which is how a readback arrives.
    fn flipped(w: usize, h: usize, colour: impl Fn(usize, usize) -> [u8; 3]) -> Vec<u8> {
        frame(w, h, |x, y| colour(x, h - 1 - y))
    }

    fn layout(w: usize, h: usize) -> Layout {
        Layout {
            alloc: (
                u32::try_from(w).expect("test width"),
                u32::try_from(h).expect("test height"),
            ),
            reference: (w, h),
            device_scale: 1,
            peak_row_tolerance: 1,
        }
    }

    /// A pattern with a bright row and a varying background, so it is neither
    /// uniform nor peak-ambiguous.
    fn trace(bright_row: usize) -> impl Fn(usize, usize) -> [u8; 3] {
        move |x, y| {
            if y == bright_row {
                [0xff, 0xff, 0xff]
            } else {
                [u8::try_from(x % 7).unwrap_or(0), 0x10, 0x20]
            }
        }
    }

    /// **The statistic is per channel, not lumped** (#893, Annika's answer 4).
    ///
    /// The regression this pins is the reason it exists: one shared population
    /// over R, G and B divides a single-channel drift by three, and the CRT
    /// skin's ink is `[0x5c, 0xff, 0x82]`, so a green-only error passes a
    /// ceiling it misses threefold.
    ///
    /// **Falsified** by pushing all three channels into one `Vec` and reporting
    /// that: the green mean below reads `2.0` instead of `6.0` and
    /// `verdict()` flips to `Pass`.
    #[test]
    fn a_single_channel_drift_is_not_diluted_by_the_other_two() {
        let (w, h) = (16, 8);
        let cpu = frame(w, h, trace(3));
        // Green is 6/255 off everywhere; red and blue are exact.
        let gl = flipped(w, h, |x, y| {
            let c = trace(3)(x, y);
            [c[0], c[1].saturating_sub(6), c[2]]
        });

        let stats = compare(&gl, &cpu, layout(w, h));
        assert_eq!(stats.pixels, w * h, "pixels, not bytes and not samples");
        assert!((stats.channels[0].mean - 0.0).abs() < 1e-9, "R is exact");
        assert!((stats.channels[1].mean - 6.0).abs() < 1e-9, "G is 6 off");
        assert!((stats.channels[2].mean - 0.0).abs() < 1e-9, "B is exact");
        assert!((stats.worst_mean() - 6.0).abs() < 1e-9, "the worst channel");
        assert_eq!(
            stats.verdict_for(Kind::Scope),
            Verdict::OverCeiling,
            "6 > the ceiling of 2, on one channel"
        );
        // The lumped statistic this replaced: (0 + 6 + 0) / 3.
        let lumped = stats.channels.iter().map(|c| c.mean).sum::<f64>() / 3.0;
        assert!(lumped <= CEILING_MEAN, "…and lumping would have passed it");
    }

    /// An identical pair is inside the ceiling on every channel, with the beam
    /// where the kit put it — the premise every other case is measured against.
    #[test]
    fn an_identical_frame_passes_on_every_channel() {
        let (w, h) = (16, 8);
        let cpu = frame(w, h, trace(2));
        let gl = flipped(w, h, trace(2));
        let stats = compare(&gl, &cpu, layout(w, h));
        assert_eq!(stats.verdict_for(Kind::Scope), Verdict::Pass);
        assert_eq!(stats.peak_row_mismatches, 0);
        assert!(!stats.uniform);
        for (channel, name) in stats.channels.iter().zip(super::CHANNELS) {
            assert!(
                (channel.mean, channel.p99, channel.max) == (0.0, 0.0, 0.0),
                "{name} should be exact, got {channel:?}"
            );
        }
    }

    /// **A blank framebuffer fails.** `GlSurface::draw` returns silently on an
    /// unregistered program, an unset state and a failed shader build, and in
    /// all three the area reports no error, a live context and a clean
    /// `glGetError` — so every other guard in the harness passes and the
    /// numbers below would be reported against pixels nothing ever wrote.
    ///
    /// **Falsified** by dropping `uniform` from `verdict()`: an all-black
    /// readback against a real frame then reports whatever the deltas happen to
    /// be — large here, but silently, and `PASS` for a dark skin.
    #[test]
    fn an_undrawn_framebuffer_fails_rather_than_reporting_numbers() {
        let (w, h) = (16, 8);
        let cpu = frame(w, h, trace(4));
        // A *flat colour* that is not black: uniform, but something was drawn.
        let flat = flipped(w, h, |_, _| [0x20, 0x00, 0x40]);
        let stats = compare(&flat, &cpu, layout(w, h));
        assert!(stats.uniform, "every pixel the same colour");
        assert!(!stats.all_zero, "…but not zero");
        assert_eq!(stats.verdict_for(Kind::Scope), Verdict::UndrawnFramebuffer);
    }

    /// **"Renders nothing" is a failure, and it is named** — #1070's review
    /// finding M2, folded in here by #1072.
    ///
    /// A glvnd stub for an unimplemented entry point returns `0` without
    /// trapping, so a wrong dispatch table degrades to an empty compile log or
    /// a no-op texture allocation: a black chip with a live context, no
    /// `GLArea::error()` and a clean `glGetError`.
    ///
    /// The dark half below is the load-bearing one: its reference differs from
    /// black by 2/255, which sits inside every one of mean, p99 and max — so
    /// **the deltas do not catch it**, and the test asserts that before
    /// asserting the verdict. What does catch it is the flat-frame family of
    /// guards, and this test pins *which member* answers: `RendersNothing`,
    /// not `UndrawnFramebuffer`.
    ///
    /// **Falsified** by dropping the `all_zero` branch from `verdict()`: the
    /// dark half then reports `UndrawnFramebuffer`. Measured, and worth being
    /// precise about, because #1072's first draft of this doc claimed it would
    /// report `PASS` and #1072's review showed otherwise — `all_zero` is a
    /// strict subset of `uniform`, and `uniform` was already checked ahead of
    /// the ceiling on `main`. The variant buys the diagnosis, not the catch.
    /// The mutation that *does* produce `PASS` here is dropping the `uniform`
    /// branch **and** the `all_zero` one; either alone still reds.
    #[test]
    fn an_all_black_gl_output_is_a_failure_even_against_a_dark_reference() {
        let (w, h) = (16, 8);
        let black = vec![0u8; w * h * 4];

        let bright = compare(&black, &frame(w, h, trace(4)), layout(w, h));
        assert!(bright.all_zero, "every compared pixel is 0,0,0");
        assert_eq!(bright.verdict_for(Kind::Scope), Verdict::RendersNothing);

        // The dangerous half: a nearly-black reference, where the *deltas*
        // cannot tell "drew the dark skin correctly" from "drew nothing" —
        // only the flat-frame guards can, and `all_zero` is which of them
        // answers.
        let dark = compare(&black, &frame(w, h, |_, _| [2, 1, 2]), layout(w, h));
        assert!(
            dark.worst_mean() <= CEILING_MEAN
                && dark.worst_p99() <= super::CEILING_P99
                && dark.worst_max() <= CEILING_MAX,
            "the premise: inside every ceiling — {:?}",
            dark.channels
        );
        assert_eq!(dark.peak_row_mismatches, 0, "…and no beam moved");
        assert_eq!(
            dark.verdict_for(Kind::Scope),
            Verdict::RendersNothing,
            "…yet it drew nothing, and that is a failure"
        );

        // A readback that never arrived at all reads the same way: the GL arm
        // put no pixels in front of the comparison either way.
        let nothing = compare(&[], &frame(w, h, trace(4)), layout(w, h));
        assert_eq!(nothing.verdict_for(Kind::Scope), Verdict::RendersNothing);
    }

    /// The worst pixel is reported with its coordinates, its channel and both
    /// arms' colours — the readout that makes a classification possible.
    ///
    /// **Falsified** by dropping the worst-pixel branch from `compare`
    /// (`worst` is `None`), or by reporting the wrong member of the pair — see
    /// the tie test below, which is the half that pins *which* pixel.
    #[test]
    fn the_worst_pixel_is_located_with_its_channel_and_both_colours() {
        let (w, h) = (16, 8);
        let cpu = frame(w, h, |_, _| [10, 20, 30]);
        let gl = flipped(w, h, |x, y| {
            if (x, y) == (5, 3) {
                [10, 20, 130]
            } else {
                [10, 20, 30]
            }
        });
        let stats = compare(&gl, &cpu, layout(w, h));
        let worst = stats.worst.expect("a non-empty comparison has one");
        assert_eq!((worst.x, worst.y), (5, 3), "reference-frame, top-down");
        assert_eq!(super::CHANNELS[worst.channel], "B");
        assert_eq!(worst.delta, 100);
        assert_eq!(worst.gl, [10, 20, 130]);
        assert_eq!(worst.cpu, [10, 20, 30]);
        assert!(
            compare(&[], &cpu, layout(0, 0)).worst.is_none(),
            "…and an empty comparison has none"
        );
    }

    /// **A tie reports the first pixel in the scan's own order**, and that
    /// order is column-major (x outer).
    ///
    /// #1072's review measured the previous version of this claim and it did
    /// not hold: the doc said `>=` for `>` would move the coordinates "to the
    /// last tie rather than the first", but the fixture planted a *unique*
    /// maximum, so there was no tie for the mutation to move and all 22 tests
    /// stayed green. A falsification note that survives its own mutation is
    /// worse than none. This one plants a real tie.
    ///
    /// Two pixels, same `|Δ|`, chosen so the two candidate scan orders
    /// disagree about which comes first: `(2, 5)` wins column-major (smaller
    /// `x`), `(9, 1)` wins row-major (smaller `y`). So the assertion pins the
    /// tie-break *and* the traversal at once — which matters because the
    /// reported coordinate is what a transcript gets triaged from, and an
    /// unstable one sends the reader to the wrong pixel.
    ///
    /// **Falsified**, measured, two ways: `delta > held.delta` → `>=` in
    /// `compare` reports `(9, 1)`; swapping `compare`'s loop nesting to y-outer
    /// reports `(9, 1)` as well.
    #[test]
    fn a_tie_reports_the_first_pixel_the_column_major_scan_reaches() {
        let (w, h) = (16, 8);
        let cpu = frame(w, h, |_, _| [10, 20, 30]);
        // `(2, 5)` is first by column; `(9, 1)` is first by row. Same delta.
        let gl = flipped(w, h, |x, y| {
            if (x, y) == (2, 5) || (x, y) == (9, 1) {
                [10, 20, 130]
            } else {
                [10, 20, 30]
            }
        });
        let worst = compare(&gl, &cpu, layout(w, h))
            .worst
            .expect("two candidates, one report");
        assert_eq!(worst.delta, 100, "the premise: the two are tied");
        assert_eq!(
            (worst.x, worst.y),
            (2, 5),
            "column-major reaches (2, 5) first; row-major would say (9, 1), \
             and `>=` would say (9, 1) too",
        );
    }

    /// The delta map is the per-pixel worst channel, in the reference frame's
    /// own top-down order — so a diff image lines up with the reference image
    /// pixel for pixel rather than being vertically mirrored.
    ///
    /// **Falsified** by dropping the flip out of `gl_pixel`: the lit pixel in
    /// the map moves to row `h - 1 - y`.
    #[test]
    fn the_delta_map_is_top_down_and_per_pixel() {
        let (w, h) = (8, 4);
        let cpu = frame(w, h, |_, _| [0, 0, 0]);
        let gl = flipped(w, h, |x, y| {
            if (x, y) == (2, 1) {
                [0, 77, 0]
            } else {
                [0, 0, 0]
            }
        });
        let map = super::delta_map(&gl, &cpu, layout(w, h));
        assert_eq!(map.len(), w * h, "one byte per pixel");
        assert_eq!(map[w + 2], 77, "at the reference's own (2, 1)");
        assert_eq!(map.iter().filter(|d| **d > 0).count(), 1);

        // …and the same flip for the image the harness writes beside it.
        let image = super::gl_image(&gl, layout(w, h));
        assert_eq!(image.len(), w * h * 3, "RGB, no alpha");
        assert_eq!(&image[(w + 2) * 3..(w + 2) * 3 + 3], &[0, 77, 0]);
    }

    /// **A beam in the wrong place fails**, even when every channel is inside
    /// the ceiling. The trace is one row tall in a mostly-flat frame, so moving
    /// it barely moves a per-pixel mean — which is exactly why the structural
    /// check has to be *in* the verdict rather than printed beside it.
    ///
    /// **Falsified** by dropping the `peak_row_mismatches` branch from
    /// `verdict()`: this case reports `PASS`.
    #[test]
    fn a_beam_in_the_wrong_row_fails_even_inside_the_ceiling() {
        // A dim one-row trace in a tall dark frame: moving it disagrees on two
        // rows out of 256, by 30/255 — inside every one of mean, p99 and max.
        let (w, h) = (16, 256);
        let dim = |row: usize| {
            move |_x: usize, y: usize| {
                if y == row { [30, 30, 30] } else { [0, 0, 0] }
            }
        };
        let cpu = frame(w, h, dim(10));
        let gl = flipped(w, h, dim(200));
        let stats = compare(&gl, &cpu, layout(w, h));
        assert_eq!(
            stats.peak_row_mismatches,
            u32::try_from(w).expect("test width"),
            "every column's peak moved"
        );
        assert!(
            stats.worst_mean() <= CEILING_MEAN
                && stats.worst_p99() <= super::CEILING_P99
                && stats.worst_max() <= CEILING_MAX,
            "…while every channel stays inside the ceiling: {:?}",
            stats.channels
        );
        assert_eq!(stats.verdict_for(Kind::Scope), Verdict::BeamMoved);
    }

    /// A peak that moved by less than the glow's own height is rounding, not a
    /// structural difference — the tolerance exists so a one-row rounding
    /// disagreement does not read as a moved beam.
    #[test]
    fn a_peak_inside_the_tolerance_is_not_a_mismatch() {
        let (w, h) = (16, 32);
        let cpu = frame(w, h, trace(10));
        let gl = flipped(w, h, trace(11));
        let stats = compare(&gl, &cpu, layout(w, h));
        assert_eq!(stats.peak_row_mismatches, 0, "one row is inside tolerance");
    }

    /// **The beam tolerance is computed, not written down at the call site.**
    ///
    /// This is the test that was missing, and its absence is exactly why the
    /// harness shipped a tolerance neither of its two comments described: every
    /// other test here passes `peak_row_tolerance` as a literal it chose
    /// itself, so the expression that produces the real one had no witness at
    /// all. This one goes through [`Layout::for_capture`], the same path the
    /// verdict uses.
    ///
    /// **Falsified** by putting the device scale back into
    /// [`peak_row_tolerance`] (`* device_scale`), or by changing
    /// [`PEAK_ROW_TOLERANCE_GRID_ROWS`].
    #[test]
    fn the_beam_tolerance_is_one_grid_row_and_ignores_the_device_scale() {
        // One grid row, in reference pixels — so it tracks the kit's upscale.
        assert_eq!(
            peak_row_tolerance(1),
            1,
            "a 1x kit frame: one row is one px"
        );
        assert_eq!(peak_row_tolerance(2), 2, "the harness's own SCALE");
        assert_eq!(peak_row_tolerance(3), 3);
        assert_eq!(
            peak_row_tolerance(0),
            1,
            "a nonsense upscale still allows one"
        );

        // **The verdict must not depend on the monitor.** Both peak rows come
        // out of `compare` in reference-frame space — `gl_at` has already
        // divided the device scale out — so a second device-scale factor here
        // would make the same pixels pass on one screen and fail on another.
        let at = |device_scale| {
            Layout::for_capture((96, 48), (48, 24), device_scale, 2).peak_row_tolerance
        };
        assert_eq!(at(1), at(2), "1x and 2x agree");
        assert_eq!(at(1), at(3), "…and 3x");
        assert_eq!(at(1), peak_row_tolerance(2), "…on the computed value");
    }

    /// The tolerance is a real boundary, driven end to end through
    /// [`Layout::for_capture`]: a beam that moved by exactly it is rounding, one
    /// row further is a beam somewhere else.
    ///
    /// **Falsified** by widening [`PEAK_ROW_TOLERANCE_GRID_ROWS`] to `2`, which
    /// makes the second half pass.
    #[test]
    fn a_beam_exactly_on_the_tolerance_passes_and_one_row_further_does_not() {
        const UPSCALE: usize = 2;
        // Tall enough that the two rows a moved trace disagrees on stay under
        // the 99th percentile — the point here is the *structural* check, so
        // the ceiling must not fire first and mask it.
        let (w, h) = (16, 256);
        let dim = |row: usize| {
            move |_x: usize, y: usize| {
                if y == row { [30, 30, 30] } else { [0, 0, 0] }
            }
        };
        let layout = Layout::for_capture(
            (u32::try_from(w).expect("w"), u32::try_from(h).expect("h")),
            (w, h),
            1,
            UPSCALE,
        );
        let tolerance = layout.peak_row_tolerance;
        assert_eq!(
            tolerance, UPSCALE,
            "the premise: one grid row at this upscale"
        );

        let cpu = frame(w, h, dim(20));
        // Exactly on the boundary: still rounding.
        let inside = compare(&flipped(w, h, dim(20 + tolerance)), &cpu, layout);
        assert_eq!(inside.peak_row_mismatches, 0);
        assert_eq!(inside.verdict_for(Kind::Scope), Verdict::Pass);
        // One reference row further: structural.
        let outside = compare(&flipped(w, h, dim(20 + tolerance + 1)), &cpu, layout);
        assert_eq!(
            outside.peak_row_mismatches,
            u32::try_from(w).expect("w"),
            "every column"
        );
        assert_eq!(outside.verdict_for(Kind::Scope), Verdict::BeamMoved);
    }

    /// The device scale point-samples the readback back down, so a 2× capture
    /// compares against the same reference rather than against a frame of a
    /// different size.
    #[test]
    fn a_scaled_capture_is_point_sampled_back_to_the_reference() {
        let (w, h) = (8, 4);
        let cpu = frame(w, h, trace(1));
        // Every reference pixel as a 2×2 block, bottom-up.
        let gl = flipped(w * 2, h * 2, |x, y| trace(1)(x / 2, y / 2));
        let stats = compare(
            &gl,
            &cpu,
            Layout {
                alloc: (16, 8),
                reference: (w, h),
                device_scale: 2,
                peak_row_tolerance: 2,
            },
        );
        assert_eq!(stats.verdict_for(Kind::Scope), Verdict::Pass);
        assert_eq!(stats.pixels, w * h);
    }

    /// `p99` is the 99th percentile and `max` the worst pixel, and they are not
    /// the same number — one outlier in a hundred must not move `p99`.
    #[test]
    fn the_percentile_and_the_maximum_are_different_numbers() {
        let mut deltas: Vec<u8> = vec![0; 299];
        deltas.push(200);
        let stats = distribution(&mut deltas);
        assert!((stats.max - 200.0).abs() < 1e-9, "the outlier is the max");
        assert!(
            (stats.p99 - 0.0).abs() < 1e-9,
            "one in three hundred is below the 99th percentile"
        );
        assert!(stats.mean < 1.0, "and it barely moves the mean");
    }

    /// The ceiling is checked per channel: a channel that breaches any one of
    /// mean / p99 / max is outside it.
    #[test]
    fn each_of_the_three_ceilings_is_load_bearing() {
        let inside = ChannelStats {
            mean: 2.0,
            p99: 8.0,
            max: 32.0,
        };
        assert!(inside.inside_ceiling(), "exactly on every limit is inside");
        for breach in [
            ChannelStats {
                mean: 2.01,
                ..inside
            },
            ChannelStats {
                p99: 8.01,
                ..inside
            },
            ChannelStats {
                max: CEILING_MAX + 0.01,
                ..inside
            },
        ] {
            assert!(!breach.inside_ceiling(), "{breach:?} is outside");
        }
        let stats = Stats {
            channels: [inside, inside, inside],
            pixels: 1,
            peak_row_mismatches: 0,
            uniform: false,
            ..Stats::default()
        };
        assert_eq!(stats.verdict_for(Kind::Scope), Verdict::Pass);
    }

    /// Every verdict prints a distinct word, and only one of them is a pass —
    /// the transcript goes on #893 and "which way did it fail" is its value.
    #[test]
    fn every_verdict_is_named_and_only_one_passes() {
        let all = [
            Verdict::Pass,
            Verdict::RendersNothing,
            Verdict::UndrawnFramebuffer,
            Verdict::OverCeiling,
            Verdict::BeamMoved,
            Verdict::NotBitExact,
        ];
        let labels: Vec<&str> = all.iter().map(|v| v.label()).collect();
        let mut unique = labels.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), all.len(), "distinct labels: {labels:?}");
        assert_eq!(
            all.iter().filter(|v| v.is_pass()).count(),
            1,
            "exactly one verdict is a pass"
        );
    }
}
