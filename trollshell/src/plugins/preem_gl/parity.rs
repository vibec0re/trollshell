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
/// of the four ways did it fail" is the whole value of pasting it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Inside the ceiling on every channel, with the beam where the kit put it.
    Pass,
    /// The readback is a single flat colour, so nothing was drawn — see
    /// [`Stats::uniform`].
    UndrawnFramebuffer,
    /// At least one channel is outside mean/p99/max.
    OverCeiling,
    /// The beam's peak row moved by more than the glow's own height in at
    /// least one column — a structural difference, not a rounding one.
    BeamMoved,
}

impl Verdict {
    /// The word the transcript prints.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::UndrawnFramebuffer => "FAIL(blank)",
            Self::OverCeiling => "FAIL(ceiling)",
            Self::BeamMoved => "FAIL(beam)",
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
    /// Columns whose brightest row moved further than the beam's own glow is
    /// tall. Zero is the only acceptable answer, and it is **in** the verdict:
    /// a trace drawn in the wrong place with the right colours can sit well
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

    /// The verdict, in the order a reader wants to be told: a blank
    /// framebuffer first (because it makes every other number meaningless),
    /// then the ceiling, then the structural check.
    pub(crate) fn verdict(&self) -> Verdict {
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
        if self.peak_row_mismatches > 0 {
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
    /// How many reference pixels the beam's peak row may move before it counts
    /// as a structural difference — the kit's glow is `2 * GLOW_SPAN + 1` rows
    /// tall in *grid* rows, so this is that times the upscale.
    pub(crate) peak_row_tolerance: usize,
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
    let (alloc_w, alloc_h) = (layout.alloc.0 as usize, layout.alloc.1 as usize);
    let scale = (layout.device_scale as usize).max(1);

    let mut deltas: [Vec<u8>; 3] = std::array::from_fn(|_| Vec::with_capacity(ref_w * ref_h));
    let mut peak_row_mismatches = 0_u32;
    let mut first_gl: Option<[u8; 3]> = None;
    let mut uniform = true;

    let gl_at = |x: usize, y: usize| -> [u8; 3] {
        // Bottom-up, and point-sampled through the device scale.
        let (sx, sy) = (x * scale, y * scale);
        let flipped = alloc_h.saturating_sub(1).saturating_sub(sy);
        let i = (flipped * alloc_w + sx) * 4;
        gl.get(i..i + 3)
            .map_or([0, 0, 0], |px| [px[0], px[1], px[2]])
    };
    let ref_at = |x: usize, y: usize| -> [u8; 3] {
        let i = (y * ref_w + x) * 4;
        reference
            .get(i..i + 3)
            .map_or([0, 0, 0], |px| [px[0], px[1], px[2]])
    };
    let luma = |px: [u8; 3]| u32::from(px[0]) + u32::from(px[1]) + u32::from(px[2]);

    for x in 0..ref_w {
        let (mut gl_peak, mut gl_peak_row) = (0, 0);
        let (mut cpu_peak, mut cpu_peak_row) = (0, 0);
        for y in 0..ref_h {
            let (g, c) = (gl_at(x, y), ref_at(x, y));
            match first_gl {
                None => first_gl = Some(g),
                Some(seen) if seen != g => uniform = false,
                Some(_) => {}
            }
            for (channel, bucket) in deltas.iter_mut().enumerate() {
                bucket.push(g[channel].abs_diff(c[channel]));
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

    Stats {
        channels: std::array::from_fn(|channel| distribution(&mut deltas[channel])),
        pixels: ref_w * ref_h,
        peak_row_mismatches,
        // An empty comparison is not "uniform", it is nothing at all — but it
        // is still not a frame, so it fails the same way.
        uniform: uniform || deltas[0].is_empty(),
    }
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
        CEILING_MAX, CEILING_MEAN, ChannelStats, Layout, Stats, Verdict, compare, distribution,
    };

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
            stats.verdict(),
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
        assert_eq!(stats.verdict(), Verdict::Pass);
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
        let blank = vec![0u8; w * h * 4];
        let stats = compare(&blank, &cpu, layout(w, h));
        assert!(stats.uniform, "every pixel the same colour");
        assert_eq!(stats.verdict(), Verdict::UndrawnFramebuffer);

        // …and so does a readback that never arrived at all.
        let nothing = compare(&[], &cpu, layout(w, h));
        assert_eq!(nothing.verdict(), Verdict::UndrawnFramebuffer);
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
        assert_eq!(stats.verdict(), Verdict::BeamMoved);
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
        assert_eq!(stats.verdict(), Verdict::Pass);
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
        };
        assert_eq!(stats.verdict(), Verdict::Pass);
    }

    /// Every verdict prints a distinct word, and only one of them is a pass —
    /// the transcript goes on #893 and "which way did it fail" is its value.
    #[test]
    fn every_verdict_is_named_and_only_one_passes() {
        let all = [
            Verdict::Pass,
            Verdict::UndrawnFramebuffer,
            Verdict::OverCeiling,
            Verdict::BeamMoved,
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
