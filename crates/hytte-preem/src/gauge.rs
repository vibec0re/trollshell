//! The **needle gauge**: a swept dial with real damped-oscillator physics —
//! the kit's analog instrument (#397, after [`Marquee`](super::Marquee) and
//! [`Scope`](super::Scope)).
//!
//! Two pieces, split the way [`PeakHold`](super::PeakHold) is split from
//! [`LedStrip`](super::LedStrip):
//!
//! - [`Needle`] — the **physics**, and nothing else. A spring-mass-damper
//!   whose deflection chases a target: it *overshoots* a step change, bounces,
//!   and settles. Pure, clock-free, and usable on its own for anything that
//!   wants instrument-grade motion.
//! - [`Gauge`] — the **face**: a swept scale arc, major/minor/mid tick marks,
//!   a lit value arc filling to the reading, and the tapered needle with its
//!   counterweight and hub, all inheriting the kit's accent tint (#376), skin
//!   palettes, bloom and [`scale`](Gauge::scale) hint (#358). It owns a
//!   [`Needle`] and renders it.
//!
//! # It is a spring, not a lerp
//!
//! The whole point of the widget. A linear interpolation (`x += (target - x) *
//! k`) is *monotone* — it can only approach its target from one side, which
//! reads as a slider, not an instrument. A real moving-coil meter is a second-
//! order system: the pointer's inertia carries it **past** the reading before
//! the damping fluid pulls it back.
//!
//! [`Needle`] models exactly that:
//!
//! ```text
//! ẍ = -ω²(x - target) - 2ζω·ẋ
//! ```
//!
//! with `ω` the undamped natural frequency ([`frequency`](Needle::frequency),
//! default [`DEFAULT_FREQ_HZ`]) and `ζ` the damping ratio
//! ([`damping`](Needle::damping), default [`DEFAULT_DAMPING`] — sub-critical,
//! so there is a visible overshoot and one small bounce before it settles).
//! The overshoot is a *checked* property, not a promise: see the module's
//! `a_lerp_cannot_overshoot_but_the_spring_does` test, which asserts the
//! non-monotone step response a lerp cannot produce.
//!
//! # Frame-rate independence
//!
//! [`advance`](Needle::advance) takes **elapsed seconds**, not a tick, and
//! integrates the oscillator with its *closed-form* solution rather than a
//! numerical step (see [`Needle::advance`]). That makes the trajectory a
//! function of wall-clock time alone: 60 steps of 1/60 s land where 15 steps
//! of 1/15 s do, to float rounding. It also removes the explosion class
//! outright — there is no timestep at which a stiff spring diverges, because
//! nothing is being integrated numerically. The kit still owns no clock (see
//! the `preem` module docs on timing): the plugin measures the elapsed time
//! and hands it over, exactly as it already owns the marquee's offset and the
//! scope's sweep cadence.
//!
//! # Motion blur
//!
//! The needle smears while it moves: [`Gauge`] draws a short fan of dimmer
//! blades at the deflections the needle held over the last
//! [`TRAIL_SPAN_SECS`], extrapolated from its velocity. Because the samples
//! are taken in *time*, the smear is frame-rate independent like the physics —
//! and because they are combined by `max`, a settled needle's samples coincide
//! with it exactly, so a resting gauge shows no trail at all.
//!
//! # Input
//!
//! Any value in the configured [`range`](Gauge::range) (default `0.0..=1.0`):
//! a battery percentage, a CPU load, a bitrate. Out-of-range targets clamp to
//! the ends and non-finite ones are ignored, so no input can throw the needle
//! off the face or poison the state. The needle itself may sit slightly past
//! full scale mid-overshoot — physically it should — and the face clamps the
//! drawn angle to [`OVERTRAVEL`] past each end, which is what a real meter's
//! mechanical stop looks like.
//!
//! ```
//! use hytte_preem::{DisplayStyle, Gauge};
//!
//! let mut gauge = Gauge::new().range(0.0, 100.0);
//! gauge.set_target(72.0);
//! // The plugin owns the clock: pass the real elapsed seconds each frame.
//! for _ in 0..90 {
//!     gauge.advance(1.0 / 60.0);
//! }
//! assert!(gauge.is_settled(), "1.5 s is plenty for the default spring");
//! let frame = gauge.render(DisplayStyle::Vfd);
//! assert_eq!(frame.data().len(), frame.width() * frame.height() * 4);
//! ```

use std::f32::consts::PI;

use super::frame::Frame;
use super::style::{DisplayStyle, Emission, mix};

// ── Physics ──────────────────────────────────────────────────────────────────

/// Default undamped natural frequency, in Hz. At `2 Hz` the needle's free
/// period is 500 ms: it reaches its first peak in ~290 ms and is settled well
/// inside a second — snappy enough for a desktop chip, slow enough to read as
/// a mechanism rather than a jump.
pub const DEFAULT_FREQ_HZ: f32 = 2.0;

/// Default damping ratio: **half critical**. Sub-critical by design — the
/// first overshoot is `exp(-πζ/√(1-ζ²))` = **16.3%** of the step, the second
/// bounce 2.7%, the third 0.4%. That is one obvious kick past the reading, one
/// small correction, then still: a moving-coil meter, not a wobble (which is
/// where `ζ ≈ 0.3` lands) and not the invisible 4.6% of `ζ = 0.7`.
pub const DEFAULT_DAMPING: f32 = 0.5;

/// Lowest accepted natural frequency, in Hz — slower than this is a needle
/// that never visibly arrives.
const MIN_FREQ_HZ: f32 = 0.05;
/// Highest accepted natural frequency, in Hz — faster than this reads as a
/// snap, and the motion blur has nothing to smear.
const MAX_FREQ_HZ: f32 = 20.0;
/// Lowest accepted damping ratio. A floor rather than `0.0`: an undamped
/// oscillator never settles, which is not an instrument.
const MIN_DAMPING: f32 = 0.05;
/// Highest accepted damping ratio — well into the overdamped, creeping regime.
const MAX_DAMPING: f32 = 4.0;

/// How close to `ζ = 1` the critically-damped (repeated-root) form is used
/// instead of the underdamped one, whose damped frequency `ω√(1-ζ²)` vanishes
/// there. Wide enough to stay clear of the singularity, narrow enough that the
/// substitution is accurate to `O(10⁻³)`.
const CRITICAL_BAND: f32 = 1.0e-3;

/// [`Needle::is_settled`]: deflection within this fraction of full scale of the
/// target …
const SETTLE_EPS: f32 = 0.002;
/// … **and** moving slower than this many fractions of full scale per second.
const SETTLE_VEL: f32 = 0.01;

/// A **damped harmonic oscillator** whose deflection chases a target: the
/// physics behind [`Gauge`]'s pointer, and usable on its own wherever
/// instrument-grade motion beats a lerp.
///
/// The state is a deflection in *fraction of full scale* (`0.0` at the low end
/// of the [`range`](Self::range), `1.0` at the high end) plus its velocity, so
/// the spring constants mean the same thing whatever units the caller reads in.
/// [`set_target`](Self::set_target) and [`value`](Self::value) do the mapping.
///
/// Sub-critically damped by default, so a step change **overshoots and
/// settles** — see the module docs on why that is not a lerp.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Needle {
    /// Deflection, in fractions of full scale. May sit slightly outside
    /// `0.0..=1.0` mid-overshoot — a real pointer does too.
    position: f32,
    /// Rate of change, in fractions of full scale per second.
    velocity: f32,
    /// Where the deflection is heading, in `0.0..=1.0`.
    target: f32,
    /// Undamped natural angular frequency `ω`, in rad/s.
    omega: f32,
    /// Damping ratio `ζ` (dimensionless); below `1.0` is underdamped.
    zeta: f32,
    /// The caller-facing scale: the value at deflection `0.0` …
    low: f32,
    /// … and at deflection `1.0`.
    high: f32,
}

impl Needle {
    /// A needle at rest at the low end of a `0.0..=1.0` scale, with the default
    /// [`frequency`](Self::frequency) and [`damping`](Self::damping).
    #[must_use]
    pub fn new() -> Self {
        Self {
            position: 0.0,
            velocity: 0.0,
            target: 0.0,
            omega: 2.0 * PI * DEFAULT_FREQ_HZ,
            zeta: DEFAULT_DAMPING,
            low: 0.0,
            high: 1.0,
        }
    }

    /// Set the undamped natural frequency in Hz — how *fast* the needle swings
    /// (its free period is `1/hz`), independent of how much it overshoots.
    /// Clamped to [`MIN_FREQ_HZ`]`..=`[`MAX_FREQ_HZ`]; a non-finite value keeps
    /// the current one. A consuming builder; call it at construction.
    #[must_use]
    pub fn frequency(mut self, hz: f32) -> Self {
        if hz.is_finite() {
            self.omega = 2.0 * PI * hz.clamp(MIN_FREQ_HZ, MAX_FREQ_HZ);
        }
        self
    }

    /// Set the damping ratio `ζ` — how much the needle *overshoots*: below
    /// `1.0` it bounces past the reading (the first overshoot is
    /// `exp(-πζ/√(1-ζ²))` of the step), at `1.0` it arrives without overshoot,
    /// above `1.0` it creeps. Clamped to [`MIN_DAMPING`]`..=`[`MAX_DAMPING`]
    /// (the floor keeps an undamped needle, which never settles, off the face);
    /// a non-finite value keeps the current one. A consuming builder.
    #[must_use]
    pub fn damping(mut self, zeta: f32) -> Self {
        if zeta.is_finite() {
            self.zeta = zeta.clamp(MIN_DAMPING, MAX_DAMPING);
        }
        self
    }

    /// Set the scale the caller reads in: `low` at the left end of the dial,
    /// `high` at the right. A degenerate (`high <= low`) or non-finite range is
    /// rejected and the current one kept — the default is `0.0..=1.0`. The
    /// physics runs in fraction-of-scale space either way, so the spring
    /// constants never need re-tuning for a new range. A consuming builder;
    /// call it at construction (it re-interprets, rather than re-maps, the
    /// deflection already held).
    #[must_use]
    pub fn range(mut self, low: f32, high: f32) -> Self {
        if low.is_finite() && high.is_finite() && high > low {
            self.low = low;
            self.high = high;
        }
        self
    }

    /// Point the needle at `value`, in the configured [`range`](Self::range).
    /// Out-of-range values clamp to the ends; a non-finite value is ignored
    /// (the needle keeps its current target rather than being poisoned).
    ///
    /// This does not move the needle — [`advance`](Self::advance) does.
    pub fn set_target(&mut self, value: f32) {
        if !value.is_finite() {
            return;
        }
        self.target = ((value - self.low) / (self.high - self.low)).clamp(0.0, 1.0);
    }

    /// Advance the needle by `dt` **seconds** of wall clock.
    ///
    /// This is the exact (closed-form) solution of `ẍ = -ω²x - 2ζωẋ` over the
    /// interval, in whichever of the three damping regimes applies, rather than
    /// a numerical integration of it. Two consequences worth stating plainly:
    ///
    /// - **Frame-rate independence is structural.** The exact solution composes
    ///   — two 8 ms steps land exactly where one 16 ms step does — so the
    ///   trajectory depends on elapsed time and nothing else. A numerical
    ///   integrator only approximates that, and the approximation is what
    ///   drifts between tick rates.
    /// - **There is no unstable timestep.** Every branch's envelope is
    ///   `exp(-ζωt)` with `ζ, ω > 0`, so the state contracts for any `dt > 0`.
    ///   A stiff spring with a huge timestep does not explode; it simply lands
    ///   on the target, which is the honest answer for a frame that took a
    ///   second.
    ///
    /// A non-positive or non-finite `dt` is a no-op. Rejecting negatives is the
    /// one real guard here: running the closed form backwards would grow the
    /// envelope instead of shrinking it.
    pub fn advance(&mut self, dt: f32) {
        // A `NaN` fails `is_finite` first, so the comparison only ever sees a
        // real number and cannot silently pass a non-finite step through.
        if !dt.is_finite() || dt <= 0.0 {
            return;
        }
        // Work in displacement-from-target space, where the system is the
        // homogeneous oscillator above and the target is just the origin.
        let decay = self.zeta * self.omega;
        let offset = self.position - self.target;
        let rate = self.velocity;
        let envelope = (-decay * dt).exp();

        let (offset, rate) = if (self.zeta - 1.0).abs() < CRITICAL_BAND {
            // Critically damped: the repeated-root solution `(A + Bt)e^{-ωt}`.
            let slope = rate + decay * offset;
            (
                envelope * (offset + slope * dt),
                envelope * (rate - decay * slope * dt),
            )
        } else if self.zeta < 1.0 {
            // Underdamped: a decaying sinusoid at the damped frequency `ω_d`.
            let damped = self.omega * (1.0 - self.zeta * self.zeta).sqrt();
            let (sin, cos) = (damped * dt).sin_cos();
            let slope = (rate + decay * offset) / damped;
            (
                envelope * (offset * cos + slope * sin),
                envelope * (rate * cos - (decay * slope + offset * damped) * sin),
            )
        } else {
            // Overdamped: two real modes, both decaying (`|ζω| > ω√(ζ²-1)`),
            // so neither exponential can grow however large `dt` is.
            let spread = self.omega * (self.zeta * self.zeta - 1.0).sqrt();
            let (fast, slow) = (-decay - spread, -decay + spread);
            let far = (rate - slow * offset) / (fast - slow);
            let near = offset - far;
            let (grow_near, grow_far) = ((slow * dt).exp(), (fast * dt).exp());
            (
                near * grow_near + far * grow_far,
                near * slow * grow_near + far * fast * grow_far,
            )
        };

        if offset.is_finite() && rate.is_finite() {
            self.position = self.target + offset;
            self.velocity = rate;
        } else {
            // Unreachable with the guards above (every input is finite and
            // every branch contracts), but a needle that lost its state should
            // park on the reading rather than render a `NaN` angle.
            self.settle();
        }
    }

    /// Snap the needle onto its target and stop it dead, keeping the
    /// [`frequency`](Self::frequency) / [`damping`](Self::damping) /
    /// [`range`](Self::range) configuration.
    ///
    /// This is the **park** primitive (#422), the [`Scope::clear`] of a gauge.
    /// An off-screen widget stops being advanced, so without it the needle
    /// freezes wherever it happened to be mid-swing and the re-shown card
    /// replays a stale animation from a stale angle. Settling instead means the
    /// reopened gauge reads the last known value immediately, and animates only
    /// the next *real* change.
    ///
    /// [`Scope::clear`]: super::Scope::clear
    pub fn settle(&mut self) {
        self.position = self.target;
        self.velocity = 0.0;
    }

    /// The needle's deflection as a fraction of full scale: `0.0` at the low end
    /// of the [`range`](Self::range), `1.0` at the high end. May sit slightly
    /// outside that mid-overshoot — the pointer really is past full scale for a
    /// moment. [`Gauge`] clamps it to the dial's travel when drawing.
    #[must_use]
    pub fn fraction(&self) -> f32 {
        self.position
    }

    /// The needle's current reading, in the configured [`range`](Self::range).
    /// Momentarily outside it during an overshoot, like [`fraction`](Self::fraction).
    #[must_use]
    pub fn value(&self) -> f32 {
        self.low + self.position * (self.high - self.low)
    }

    /// The value the needle is heading for, in the configured
    /// [`range`](Self::range) — always inside it.
    #[must_use]
    pub fn target(&self) -> f32 {
        self.low + self.target * (self.high - self.low)
    }

    /// How fast the needle is moving, in **fractions of full scale per second**
    /// (signed; positive is toward the high end). Range-independent, which is
    /// what the motion blur wants.
    #[must_use]
    pub fn velocity(&self) -> f32 {
        self.velocity
    }

    /// Whether the needle has arrived: within [`SETTLE_EPS`] of the target and
    /// moving slower than [`SETTLE_VEL`].
    ///
    /// Worth polling — a plugin can drop its frame timer while this is true and
    /// re-arm it when a new target arrives, since a settled needle renders the
    /// same frame forever.
    #[must_use]
    pub fn is_settled(&self) -> bool {
        (self.position - self.target).abs() <= SETTLE_EPS && self.velocity.abs() <= SETTLE_VEL
    }
}

impl Default for Needle {
    fn default() -> Self {
        Self::new()
    }
}

// ── Face geometry ────────────────────────────────────────────────────────────

/// Default logical buffer width (pre-upscale). 144 columns at [`DEFAULT_SCALE`]
/// render 288 px wide — inside the ~296 px sidebar card (the #313 lesson), and
/// the same width as the [`Scope`](super::Scope) it sits beside.
const DEFAULT_COLS: usize = 144;
/// Default logical buffer height (pre-upscale). 64 rows at [`DEFAULT_SCALE`]
/// render 128 px tall — a classic wide panel-meter face.
const DEFAULT_ROWS: usize = 64;
/// Default integer upscale baked into the output ([`Frame::upscale`]): chunky,
/// nearest-neighbor pixels, the kit's house look.
const DEFAULT_SCALE: usize = 2;

/// Default total sweep, in degrees — the classic panel/automotive dial arc.
const DEFAULT_SWEEP_DEG: f32 = 150.0;
/// Narrowest accepted sweep, in degrees.
const MIN_SWEEP_DEG: f32 = 10.0;
/// Widest accepted sweep, in degrees. Capped at a half turn: past that the
/// arc's ends drop *below* the pivot, which this bottom-pivot face has no room
/// for.
const MAX_SWEEP_DEG: f32 = 180.0;

/// Default major divisions (intervals between long ticks).
const DEFAULT_DIVISIONS: usize = 4;
/// Default minor ticks per major division.
const DEFAULT_SUBDIVISIONS: usize = 5;

/// Clear logical pixels kept between the scale arc and the buffer edge, so the
/// arc's soft edge never clips.
const EDGE: f32 = 3.5;
/// Rows kept below the pivot for the counterweight, as a fraction of the buffer
/// height …
const BASE_FRAC: f32 = 0.14;
/// … with this floor in logical pixels, for short buffers.
const MIN_BASE: f32 = 5.0;
/// Smallest scale-arc radius, in logical pixels — a degenerate buffer collapses
/// to this rather than to a negative radius.
const MIN_RADIUS: f32 = 1.0;

/// Needle tip radius, as a fraction of the scale-arc radius: just inside the
/// major ticks.
const TIP_FRAC: f32 = 0.80;
/// Counterweight length behind the pivot, as a fraction of the arc radius.
const TAIL_FRAC: f32 = 0.13;
/// How much the counterweight flares as it goes back: its far end is drawn this
/// multiple of the blade's pivot half-width, which is what gives the stub its
/// weight instead of reading as a second pointer.
const TAIL_FLARE: f32 = 1.25;
/// Shortest counterweight worth drawing, in logical pixels (#931).
///
/// Below this the stub sits entirely inside the hub's own feathered disc, so it
/// reads as a fatter pivot rather than as a counterweight — and a fatter pivot
/// is exactly what a small dial has no room for. Under it the counterweight is
/// dropped and the needle is a bare pointer.
///
/// Inert on every face that has the room: the default 144×64's counterweight is
/// 6.6 logical px and a 64×64 square dial's is 3.8. It fires at 48×48, where the
/// stub would be 2.7 px behind a 1.4 px hub.
const MIN_TAIL: f32 = 3.0;
/// Hub radius, as a fraction of the arc radius …
const HUB_FRAC: f32 = 0.065;
/// … with this floor in logical pixels, so a small face still has a pivot.
const MIN_HUB: f32 = 1.2;
/// Needle half-width at the pivot, as a fraction of the arc radius …
const BLADE_FRAC: f32 = 0.032;
/// … clamped into this range in logical pixels.
///
/// The floor is [`BLADE_TIP`] — a blade that has thinned to its own tip width is
/// a uniform hairline, which is the thinnest thing this face knows how to draw
/// and still the right pointer for a small dial (#931). A wider floor would
/// make the pivot end of a 48×48 dial's needle as fat as its tip is on the
/// default face: `radius * `[`BLADE_FRAC`] is `0.66` px there against a floor of
/// `0.8`, so the floor — not the fraction — would be setting the width, and the
/// pointer would read as a wedge. The fraction binds on every face down to an
/// arc radius of ~16 logical px, so this only ever *relaxes* a clamp that was
/// already inert at the default (whose blade is 1.6 px).
const BLADE_RANGE: (f32, f32) = (BLADE_TIP, 2.2);
/// Needle half-width at the tip, in logical pixels — the blade tapers to a
/// point, like a real pointer.
const BLADE_TIP: f32 = 0.5;

/// Scale-arc band half-width, in logical pixels.
const ARC_HW: f32 = 0.8;
/// How much fatter the lit value arc is drawn than the scale arc it fills.
const VALUE_HW_BONUS: f32 = 0.35;
/// Major/mid tick half-width, in logical pixels.
const MAJOR_HW: f32 = 0.85;
/// Minor tick half-width, in logical pixels.
const MINOR_HW: f32 = 0.55;
/// Major tick length inward from the scale arc, as a fraction of its radius.
const MAJOR_LEN_FRAC: f32 = 0.15;
/// Minor tick length inward from the scale arc, as a fraction of its radius.
const MINOR_LEN_FRAC: f32 = 0.075;
/// Shortest major (and mid) tick, in logical pixels (#931) — under it the mark
/// is shorter than the [`FEATHER`] ramp on either end of it and reads as a
/// smudge on the arc rather than as a division boundary.
///
/// Inert at **all three** tuned faces — the default's major is 7.6 px, 64×64's
/// is 4.3 and even 48×48's is 3.11 — so it first fires around a 40×40 buffer.
/// [`MIN_MINOR_LEN`] is the floor that bites at 48; this one is its argument
/// carried to the longer mark, and it is here so a face small enough to need
/// both keeps the two lengths apart.
const MIN_MAJOR_LEN: f32 = 3.0;
/// Shortest minor tick, in logical pixels (#931) — the [`MIN_MAJOR_LEN`]
/// argument, one notch down, so a subdivision stays visibly shorter than the
/// boundary it sits between. Inert at the default (3.8 px) and at 64×64
/// (2.2 px); it fires at 48×48, where the natural minor is 1.6 px.
const MIN_MINOR_LEN: f32 = 2.0;
/// How much longer the mid-scale tick is drawn than a major one.
const MID_LEN_BONUS: f32 = 1.35;
/// Closest two adjacent ticks may sit on the scale arc, centre to centre, in
/// logical pixels (#931). Below it the face drops a subdivision level rather
/// than drawing a row of marks that merge into a band.
///
/// A minor tick lays down `2 * (`[`MINOR_HW`]` + `[`FEATHER`]`)` ≈ 3.4 logical
/// px of ink at the shipped constants, so 5.9 leaves ~2.5 clear px between
/// neighbours — a gap that survives the ×2 upscale as five screen pixels of
/// field. It is written as a constant rather than derived from those two so
/// that a change to the kit's anti-aliasing ramp cannot silently re-tick every
/// dial in the workspace.
///
/// Inert at the default face, whose 20 intervals sit 6.6 logical px apart; a
/// 64×64 square dial drops to 3 subdivisions and a 48×48 to 2.
const MIN_TICK_SPACING: f32 = 5.9;
/// The widest bloom a face may spend, as a divisor of its arc radius (#931).
///
/// The kit bakes chunkiness into the *logical* buffer and upscales last
/// ([`Gauge::render`] ends in [`Frame::upscale`]), so a skin's bloom radius is a
/// fixed count of logical px however small the dial is: `Vfd`'s `2` is 4 % of
/// the default face's 50.5 px arc radius but 10 % of a 48×48 square dial's 20.7
/// — the halo grows relative to the needle exactly as the dial shrinks, which is
/// the "too blurry" complaint scaled down. Capping the radius at a sixteenth of
/// the arc holds the proportion instead.
///
/// At the default face the cap is `⌊50.5 / 16⌋ = 3`, which is the widest radius
/// any skin asks for (`Crt`'s), so the default and every face with an arc radius
/// of 48 logical px or more takes no cap at all. Nothing here touches the skin
/// palettes themselves — the [`Scope`](super::Scope) beside a gauge keeps its
/// halo exactly as it is.
const BLOOM_ARC_DIV: f32 = 16.0;

/// How far a lit edge ramps from full intensity to nothing, perpendicular to
/// the shape, in logical pixels.
///
/// This is the kit's anti-aliasing, and the arbitrary-angle counterpart of the
/// [`Scope`](super::Scope)'s fixed vertical glow kernel: the scope's trace is
/// near-horizontal so a hard edge with a 5-tap vertical falloff reads as a soft
/// beam, but a needle sits at *any* angle, where a hard 1 px line stair-steps.
/// Every shape on the face — arc, tick, blade, hub — gets its coverage from the
/// same distance ramp, so nothing on the dial looks hand-drawn next to anything
/// else.
const FEATHER: f32 = 1.15;

/// The dial's mechanical stops: how far past each end of the scale the needle
/// is allowed to travel. A real meter's pointer bangs its stop on a full-scale
/// slam; this is where that happens. The *physics* is never clamped — only the
/// drawn angle.
pub const OVERTRAVEL: f32 = 0.06;

// Intensities, of 255. The face's flat furniture is mixed from the field toward
// the ink, exactly like the `Scope`'s graticule — it is a stable reference, so
// it must not bloom, decay or flicker. Everything in the *lit* layer does bloom.
/// Scale arc, flat.
const ARC_T: u16 = 30;
/// Minor tick, flat.
const MINOR_T: u16 = 52;
/// Major tick, flat.
const MAJOR_T: u16 = 86;
/// Mid-scale tick, flat — brighter than a major, the way the scope's center
/// cross is brighter than its grid.
const MID_T: u16 = 118;
/// The lit value arc filling the scale to the reading.
const VALUE_T: u16 = 130;
/// The needle blade and counterweight — full brightness.
const NEEDLE_T: u16 = 255;
/// The pivot hub, a hair under the blade so the blade still reads over it.
const HUB_T: u16 = 235;
/// The motion-blur blades, oldest last. Length sets [`TRAIL_SPAN_SECS`]'s
/// subdivision.
const TRAIL_T: [u16; 4] = [150, 104, 66, 34];

/// How far back in time the needle's motion blur reaches, in seconds — a ~50 ms
/// shutter. Sampling the smear in *time* rather than in past frames is what
/// keeps it frame-rate independent along with the physics.
pub const TRAIL_SPAN_SECS: f32 = 0.05;

/// What a tick mark on the face is worth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tick {
    /// A subdivision: short and faint.
    Minor,
    /// A division boundary: long and clear.
    Major,
    /// Mid-scale: the longest and brightest, the dial's own center reference.
    Mid,
}

/// The resolved pixel geometry of one dial face, derived from the buffer size
/// and sweep. All lengths are in logical (pre-upscale) pixels.
#[derive(Debug, Clone, Copy)]
struct Dial {
    /// The pivot, in buffer coordinates.
    pivot: (f32, f32),
    /// The scale arc's radius.
    radius: f32,
    /// Half the total sweep, in radians: the dial spans `-half..=half` measured
    /// from 12 o'clock, growing clockwise.
    half: f32,
    /// Needle tip radius.
    tip: f32,
    /// Counterweight radius, behind the pivot — `0.0` when the face is too
    /// small to draw one (see [`MIN_TAIL`]).
    tail: f32,
    /// Hub radius.
    hub: f32,
    /// Blade half-width at the pivot (it tapers to [`BLADE_TIP`]).
    blade: f32,
    /// Major tick length, inward from the scale arc.
    major_len: f32,
    /// Minor tick length, inward from the scale arc.
    minor_len: f32,
    /// Whether the face was **centred** in its buffer rather than seated on the
    /// rows [`BASE_FRAC`] reserves (#931). True exactly when the arc fits by
    /// width *and* the buffer can hold the halo at both ends — see
    /// [`Gauge::dial`].
    ///
    /// Test-only: the renderer needs the resolved `pivot`, never the branch that
    /// produced it. It is carried on the [`Dial`] rather than recomputed in
    /// `a_centred_face_leaves_room_for_its_own_halo` because a test that
    /// re-derives the predicate is an oracle that agrees with the code by
    /// construction.
    #[cfg(test)]
    centred: bool,
    /// Minor ticks per major division **as the face can actually draw them**:
    /// the configured count, pulled down until adjacent ticks clear
    /// [`MIN_TICK_SPACING`]. Resolved here rather than in
    /// [`Gauge::ticks`](Gauge::ticks) because it is a function of the arc's
    /// radius, which only the resolved geometry knows.
    subdivisions: usize,
}

impl Dial {
    /// The dial angle for a `0.0..=1.0` fraction of full scale: the low end sits
    /// at `-half`, the high end at `+half`.
    fn angle(self, fraction: f32) -> f32 {
        (fraction - 0.5) * 2.0 * self.half
    }
}

// ── The widget ───────────────────────────────────────────────────────────────

/// A **needle gauge** tile: a swept scale arc with tick marks, a lit value arc,
/// and a damped pointer that overshoots and settles.
///
/// Holds its own [`Needle`] — [`set_target`](Self::set_target) it, then
/// [`advance`](Self::advance) it with the elapsed seconds each frame and
/// [`render`](Self::render) it into the view (or [`tick`](Self::tick) for both
/// at once). The skin is taken at *render* time, not construction, so a live
/// host re-tint (#376) or a plugin's own rotation can re-skin a swinging needle
/// without disturbing its motion — the same reason [`Scope`](super::Scope) does
/// it that way.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Gauge {
    /// Logical buffer width in columns (pre-upscale).
    cols: usize,
    /// Logical buffer height in rows (pre-upscale).
    rows: usize,
    /// Integer upscale baked into the rendered [`Frame`].
    scale: usize,
    /// Total sweep of the scale, in radians.
    sweep: f32,
    /// Major divisions (intervals between long ticks).
    divisions: usize,
    /// Minor ticks per major division.
    subdivisions: usize,
    /// The pointer's physics.
    needle: Needle,
}

impl Gauge {
    /// A gauge at the default geometry ([`DEFAULT_COLS`]×[`DEFAULT_ROWS`] at
    /// [`DEFAULT_SCALE`]) and sweep, its needle at rest at the low end of a
    /// `0.0..=1.0` scale.
    #[must_use]
    pub fn new() -> Self {
        Self::with_size(DEFAULT_COLS, DEFAULT_ROWS)
    }

    /// A gauge with an explicit **logical** buffer size (pre-upscale), clamped
    /// to at least 1×1. The rendered frame is `width`×`scale` by `height`×`scale`
    /// px — keep it within the ~296 px sidebar card (the default is 288 px wide).
    #[must_use]
    pub fn with_size(width: usize, height: usize) -> Self {
        Self {
            cols: width.max(1),
            rows: height.max(1),
            scale: DEFAULT_SCALE,
            sweep: DEFAULT_SWEEP_DEG.to_radians(),
            divisions: DEFAULT_DIVISIONS,
            subdivisions: DEFAULT_SUBDIVISIONS,
            needle: Needle::new(),
        }
    }

    /// Set the integer upscale baked into the output (clamped to at least 1) —
    /// the kit bakes chunkiness into the buffer rather than leaning on shell CSS
    /// (the `.caw-lcd` lesson). A consuming builder; call it at construction.
    #[must_use]
    pub fn scale(mut self, factor: usize) -> Self {
        self.scale = factor.max(1);
        self
    }

    /// Set the total sweep of the scale in degrees, clamped to
    /// [`MIN_SWEEP_DEG`]`..=`[`MAX_SWEEP_DEG`]; a non-finite value keeps the
    /// current one. Default: [`DEFAULT_SWEEP_DEG`]. A consuming builder.
    #[must_use]
    pub fn sweep_deg(mut self, degrees: f32) -> Self {
        if degrees.is_finite() {
            self.sweep = degrees.clamp(MIN_SWEEP_DEG, MAX_SWEEP_DEG).to_radians();
        }
        self
    }

    /// Set the tick layout: `divisions` major intervals, each cut into
    /// `subdivisions` minor steps (both clamped to at least 1). Defaults:
    /// [`DEFAULT_DIVISIONS`] and [`DEFAULT_SUBDIVISIONS`]. A consuming builder.
    #[must_use]
    pub fn ticks(mut self, divisions: usize, subdivisions: usize) -> Self {
        self.divisions = divisions.max(1);
        self.subdivisions = subdivisions.max(1);
        self
    }

    /// [`Needle::frequency`], on the gauge's own pointer. A consuming builder.
    #[must_use]
    pub fn frequency(mut self, hz: f32) -> Self {
        self.needle = self.needle.frequency(hz);
        self
    }

    /// [`Needle::damping`], on the gauge's own pointer. A consuming builder.
    #[must_use]
    pub fn damping(mut self, zeta: f32) -> Self {
        self.needle = self.needle.damping(zeta);
        self
    }

    /// [`Needle::range`], on the gauge's own pointer. A consuming builder.
    #[must_use]
    pub fn range(mut self, low: f32, high: f32) -> Self {
        self.needle = self.needle.range(low, high);
        self
    }

    /// The pointer, for the full [`Needle`] surface
    /// ([`value`](Needle::value), [`target`](Needle::target),
    /// [`velocity`](Needle::velocity), …).
    #[must_use]
    pub fn needle(&self) -> &Needle {
        &self.needle
    }

    /// [`Needle::set_target`]: point the needle at `value`, in the configured
    /// [`range`](Self::range). This does not move it — [`advance`](Self::advance)
    /// does.
    pub fn set_target(&mut self, value: f32) {
        self.needle.set_target(value);
    }

    /// [`Needle::advance`]: move the needle by `dt` **seconds** of wall clock.
    /// The kit owns no clock; pass the real elapsed time (see the module docs).
    pub fn advance(&mut self, dt: f32) {
        self.needle.advance(dt);
    }

    /// [`Needle::settle`]: snap the needle onto its reading and stop it dead —
    /// the **park** primitive (#422) for an off-screen gauge. Keeps the
    /// geometry, [`scale`](Self::scale), [`ticks`](Self::ticks) and spring
    /// configuration a rebuilt [`new`](Self::new) would have thrown away.
    ///
    /// ```
    /// use hytte_preem::{DisplayStyle, Gauge};
    ///
    /// let mut gauge = Gauge::new();
    /// let rest = gauge.render(DisplayStyle::Vfd);
    /// gauge.set_target(1.0);
    /// gauge.advance(0.1);
    /// assert_ne!(gauge.render(DisplayStyle::Vfd), rest, "the needle swung");
    /// gauge.settle();
    /// assert!(gauge.is_settled(), "parked on the reading, not mid-swing");
    /// ```
    pub fn settle(&mut self) {
        self.needle.settle();
    }

    /// [`Needle::is_settled`]: whether the pointer has arrived and stopped.
    #[must_use]
    pub fn is_settled(&self) -> bool {
        self.needle.is_settled()
    }

    /// [`Needle::value`]: the current reading, in the configured
    /// [`range`](Self::range).
    #[must_use]
    pub fn value(&self) -> f32 {
        self.needle.value()
    }

    /// [`Needle::fraction`]: the current deflection as a fraction of full scale.
    #[must_use]
    pub fn fraction(&self) -> f32 {
        self.needle.fraction()
    }

    /// The rendered frame width in px (logical columns × [`scale`](Self::scale)).
    #[must_use]
    pub fn width(&self) -> usize {
        self.cols * self.scale
    }

    /// The rendered frame height in px (logical rows × [`scale`](Self::scale)).
    #[must_use]
    pub fn height(&self) -> usize {
        self.rows * self.scale
    }

    /// [`advance`](Self::advance) then [`render`](Self::render) in one call —
    /// the convenience for a plugin that moves and re-renders on the same frame.
    #[must_use]
    pub fn tick(&mut self, dt: f32, style: DisplayStyle) -> Frame {
        self.advance(dt);
        self.render(style)
    }

    /// Compose the current frame in `style`: the flat dial face (scale arc and
    /// ticks, painted like the [`Scope`](super::Scope)'s graticule so they never
    /// bloom or flicker) under the lit layer — the value arc, the needle's
    /// motion blur, its tapered blade, counterweight and hub — bloomed and
    /// composited toward the skin's accent-tinted ink (#376), then upscaled by
    /// [`scale`](Self::scale). The buffer is fully opaque and always satisfies
    /// the host's `len == w * h * 4` invariant.
    #[must_use]
    pub fn render(&self, style: DisplayStyle) -> Frame {
        let dial = self.dial();
        let mut palette = style.palette();
        // A smaller dial gets a proportionally tighter halo (#931): the bloom
        // runs at logical resolution and the upscale is the last thing `render`
        // does, so an uncapped radius is a fixed pixel count that swallows a
        // small face's needle. Local to this frame — the skin's own palette,
        // and every other kit widget wearing it, is untouched.
        if let Some(bloom) = palette.bloom.as_mut() {
            bloom.radius = bloom.radius.min(bloom_cap(dial.radius));
        }
        let mut frame = Frame::filled(self.cols, self.rows, palette.bg);

        // ── The face: flat furniture, mixed from the field toward the ink. It
        // is a stable reference like the scope's graticule, so it is redrawn
        // flat every frame and never joins the lit layer.
        let mut face = Grid::new(self.cols, self.rows);
        face.arc(
            dial.pivot,
            dial.radius,
            ARC_HW,
            -dial.half,
            dial.half,
            ARC_T,
        );
        for (fraction, kind) in self.tick_marks(dial) {
            let (length, half_width, intensity) = match kind {
                Tick::Minor => (dial.minor_len, MINOR_HW, MINOR_T),
                Tick::Major => (dial.major_len, MAJOR_HW, MAJOR_T),
                // Capped at the radius for the same reason the floors are: a
                // degenerate face must not draw a tick out through its pivot.
                Tick::Mid => (
                    (dial.major_len * MID_LEN_BONUS).min(dial.radius),
                    MAJOR_HW,
                    MID_T,
                ),
            };
            let theta = dial.angle(fraction);
            face.segment(
                polar(dial.pivot, dial.radius, theta),
                polar(dial.pivot, dial.radius - length, theta),
                half_width,
                half_width,
                intensity,
            );
        }
        for y in 0..self.rows {
            for x in 0..self.cols {
                let intensity = face.get(x, y);
                if intensity > 0 {
                    frame.set(x, y, mix(palette.bg, palette.ink, intensity));
                }
            }
        }

        // ── The lit layer, max-combined so overlapping shapes never sum into a
        // fatter, brighter blob than any of them (which is also what makes a
        // settled needle's motion blur vanish exactly).
        let mut lit = Grid::new(self.cols, self.rows);
        let reading = on_dial(self.needle.fraction());
        let filled = reading.clamp(0.0, 1.0);
        if filled > 0.0 {
            lit.arc(
                dial.pivot,
                dial.radius,
                ARC_HW + VALUE_HW_BONUS,
                dial.angle(0.0),
                dial.angle(filled),
                VALUE_T,
            );
        }
        // Motion blur first, so the blade's own full intensity wins wherever
        // they overlap.
        let step = TRAIL_SPAN_SECS / fx(TRAIL_T.len());
        for (index, &intensity) in TRAIL_T.iter().enumerate() {
            let back = fx(index + 1) * step;
            let smear = on_dial(trail_fraction(
                self.needle.fraction(),
                self.needle.velocity(),
                back,
            ));
            self.blade(&mut lit, dial, dial.angle(smear), intensity, false);
        }
        self.blade(&mut lit, dial, dial.angle(reading), NEEDLE_T, true);
        lit.segment(dial.pivot, dial.pivot, dial.hub, dial.hub, HUB_T);

        let mut emission = Emission::new(self.cols, self.rows);
        for y in 0..self.rows {
            for x in 0..self.cols {
                let intensity = lit.get(x, y);
                if intensity > 0 {
                    emission.add(x, y, intensity);
                }
            }
        }
        if let Some(bloom) = palette.bloom {
            emission.bloom(bloom);
        }
        emission.composite(&mut frame, palette.ink, palette.mask);

        frame.upscale(self.scale)
    }

    /// Stamp one pointer blade at `theta`: a tapered line from the pivot out to
    /// the tip, plus (for the live needle, not its motion blur, and only where
    /// the face has the room — see [`MIN_TAIL`]) the counterweight stub behind
    /// the pivot.
    fn blade(&self, lit: &mut Grid, dial: Dial, theta: f32, intensity: u16, weighted: bool) {
        let _ = self;
        lit.segment(
            dial.pivot,
            polar(dial.pivot, dial.tip, theta),
            dial.blade,
            BLADE_TIP,
            intensity,
        );
        if weighted && dial.tail > 0.0 {
            lit.segment(
                dial.pivot,
                polar(dial.pivot, -dial.tail, theta),
                dial.blade,
                dial.blade * TAIL_FLARE,
                intensity,
            );
        }
    }

    /// Every tick on the face, low end to high: its fraction of full scale and
    /// what it is worth.
    ///
    /// Takes the resolved [`Dial`] because the subdivision count is a property
    /// of the *face*, not of the configuration: a dial too small to separate the
    /// configured minor ticks draws fewer of them (see [`MIN_TICK_SPACING`]).
    fn tick_marks(&self, dial: Dial) -> impl Iterator<Item = (f32, Tick)> {
        let steps = self.divisions * dial.subdivisions;
        let subdivisions = dial.subdivisions;
        (0..=steps).map(move |index| {
            let kind = if index * 2 == steps {
                Tick::Mid
            } else if index % subdivisions == 0 {
                Tick::Major
            } else {
                Tick::Minor
            };
            (fx(index) / fx(steps), kind)
        })
    }

    /// Resolve the face's pixel geometry for the current buffer and sweep. The
    /// arc is fitted to whichever of the buffer's edges binds first, and every
    /// needle metric is a fraction of that radius, so a re-sized gauge stays in
    /// proportion instead of growing a hub the size of its face.
    ///
    /// # Wide faces and square ones (#931)
    ///
    /// A 150° sweep is about twice as wide as it is tall, so on the default
    /// 144×64 buffer the **height** binds: the arc is as tall as the rows allow,
    /// its apex sits exactly [`EDGE`] under the top, and the pivot is seated on
    /// the [`BASE_FRAC`] rows kept for the counterweight. That is the classic
    /// panel-meter face and nothing below changes it.
    ///
    /// On a **square** buffer — a 48×48 or 64×64 small dial — the width binds
    /// instead, and a seated pivot would leave the entire top third of the
    /// buffer empty while the needle crowds the bottom edge. So whenever the
    /// width is what fits the arc, the face is **centred** in the height it has:
    /// the pivot moves up until the drawn extent (the value arc's outer edge
    /// above, the counterweight or the bare hub below) has equal margins. The
    /// radius is unchanged by the move — it was already the width's answer, and
    /// moving the pivot up only ever grows the height's.
    ///
    /// The budget counts the **halo** [`render`](Self::render) adds afterwards
    /// ([`bloom_cap`] logical px at each end), because a face centred on its ink
    /// alone can still have its glow cut off at the top — and asymmetrically, on
    /// the side the centring moved toward. Adding the same amount at both ends
    /// leaves the pivot exactly where it was on every face with room to spare,
    /// so this costs the tuned sizes nothing. Where the buffer *cannot* hold
    /// both halos — a near-square face like `105×64`, whose arc fits by width by
    /// a hair — centring would buy nothing and cost a row of glow, so the face
    /// stays **seated**: precisely what it did before #931, which is the one
    /// placement guaranteed not to be a regression.
    fn dial(&self) -> Dial {
        let cols = fx(self.cols);
        let rows = fx(self.rows);
        let half = (self.sweep / 2.0).clamp(
            MIN_SWEEP_DEG.to_radians() / 2.0,
            MAX_SWEEP_DEG.to_radians() / 2.0,
        );
        let pivot_x = (cols - 1.0) / 2.0;
        let base = (rows * BASE_FRAC).max(MIN_BASE);
        let seated_y = (rows - 1.0 - base).max(0.0);
        // The arc's topmost point is `radius` above the pivot; its ends are
        // `radius * sin(half)` to either side. Take whichever limit binds.
        let by_height = seated_y - EDGE;
        let by_width = (pivot_x - EDGE) / half.sin().max(f32::EPSILON);
        let radius = by_height.min(by_width).max(MIN_RADIUS);
        // Width binds ⇒ there is height to spare ⇒ the face *may* be centred in
        // it. Whether it actually is depends on the halo fitting too, below.
        let spare_height = by_width < by_height;

        let blade = (radius * BLADE_FRAC).clamp(BLADE_RANGE.0, BLADE_RANGE.1);
        let hub = (radius * HUB_FRAC).max(MIN_HUB);
        // On a seated face the counterweight has to stay inside the rows
        // reserved for it; on a centred one the centring below is what reserves
        // the room, so the fraction stands. Either way a stub too short to read
        // as one is dropped.
        let stub = if spare_height {
            radius * TAIL_FRAC
        } else {
            (radius * TAIL_FRAC).min((base - FEATHER - 0.5).max(0.0))
        };
        let tail = if stub < MIN_TAIL { 0.0 } else { stub };
        // The face's vertical extent about the pivot: the *value* arc's outer
        // edge above (it is the wider of the two bands the scale carries), and
        // whichever of the counterweight or the bare hub reaches lowest below —
        // each widened by the halo `render` will lay over it.
        let glow = fx(bloom_cap(radius));
        let above = radius + ARC_HW + VALUE_HW_BONUS + FEATHER + glow;
        let below = glow
            + FEATHER
            + if tail > 0.0 {
                tail + blade * TAIL_FLARE
            } else {
                hub
            };
        let margin = (rows - 1.0 - above - below) / 2.0;
        // Centre only when the whole drawn face — halo included — fits. When it
        // does not, stay seated: that is the pre-#931 placement, so it cannot be
        // a regression, and centring a face whose glow is clipped either way
        // would only move which end loses it.
        let centred = spare_height && margin >= 0.0;
        let pivot_y = if centred {
            (margin + above).clamp(0.0, (rows - 1.0).max(0.0))
        } else {
            seated_y
        };
        Dial {
            pivot: (pivot_x, pivot_y),
            radius,
            half,
            tip: radius * TIP_FRAC,
            tail,
            hub,
            blade,
            // Ticks take a floor in logical pixels so a small dial's marks stay
            // marks, and a ceiling at the radius so a degenerate face cannot
            // draw one back out through its own pivot.
            major_len: (radius * MAJOR_LEN_FRAC).clamp(MIN_MAJOR_LEN.min(radius), radius),
            minor_len: (radius * MINOR_LEN_FRAC).clamp(MIN_MINOR_LEN.min(radius), radius),
            subdivisions: self
                .subdivisions
                .min(tick_budget(radius * 2.0 * half, self.divisions)),
            #[cfg(test)]
            centred,
        }
    }
}

impl Default for Gauge {
    fn default() -> Self {
        Self::new()
    }
}

// ── Drawing ──────────────────────────────────────────────────────────────────

/// An intensity grid (`0..=255` per logical pixel) that shapes are **max**-
/// combined into, exactly like the [`Scope`](super::Scope)'s phosphor buffer.
///
/// Max rather than add is load-bearing twice over: overlapping shapes (a tick
/// crossing the scale arc, the blade over its own counterweight) keep the
/// brighter one's edge instead of blooming into a fat saturated blob, and the
/// needle's motion blur — which is *exactly* the needle's geometry when it is
/// at rest — disappears completely on a settled gauge rather than thickening it.
struct Grid {
    width: usize,
    height: usize,
    v: Vec<u16>,
}

impl Grid {
    fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            v: vec![0u16; width * height],
        }
    }

    /// The intensity at (`x`, `y`); `0` outside the buffer.
    fn get(&self, x: usize, y: usize) -> u16 {
        if x >= self.width || y >= self.height {
            return 0;
        }
        self.v[y * self.width + x]
    }

    /// Raise (`x`, `y`) to at least `intensity`, silently clipping
    /// out-of-bounds (the same contract as [`Frame::plot`]).
    fn raise(&mut self, x: usize, y: usize, intensity: u16) {
        if x >= self.width || y >= self.height {
            return;
        }
        let cell = &mut self.v[y * self.width + x];
        *cell = (*cell).max(intensity.min(255));
    }

    /// Stamp a soft-edged line segment from `head` to `tail`, its solid
    /// half-width tapering from `wide` to `narrow` along the way. Peak
    /// intensity in the solid part, ramping to nothing over [`FEATHER`] beyond
    /// it — the shape's anti-aliasing and its soft edge in one pass.
    ///
    /// A zero-length segment is a **disc** of radius `wide`, which is how the
    /// hub is drawn.
    fn segment(&mut self, head: (f32, f32), tail: (f32, f32), wide: f32, narrow: f32, peak: u16) {
        let pad = wide.max(narrow) + FEATHER;
        let xs = span(
            head.0.min(tail.0) - pad,
            head.0.max(tail.0) + pad,
            self.width,
        );
        let ys = span(
            head.1.min(tail.1) - pad,
            head.1.max(tail.1) + pad,
            self.height,
        );
        let (run, rise) = (tail.0 - head.0, tail.1 - head.1);
        let length2 = run * run + rise * rise;
        for y in ys {
            for x in xs.clone() {
                let (px, py) = (fx(x), fx(y));
                // Where along the segment the pixel's nearest point sits.
                let along = if length2 > f32::EPSILON {
                    (((px - head.0) * run + (py - head.1) * rise) / length2).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let near = (head.0 + run * along, head.1 + rise * along);
                let distance = (px - near.0).hypot(py - near.1);
                let intensity = shade(coverage(distance, wide + (narrow - wide) * along), peak);
                if intensity > 0 {
                    self.raise(x, y, intensity);
                }
            }
        }
    }

    /// Stamp a soft-edged arc band of `radius` (± `half_width`) about `center`,
    /// between the dial angles `start` and `end`.
    ///
    /// Distance is measured to the nearest point on the arc *segment* — the
    /// pixel's angle clamped into the sweep — which gives the same soft edge all
    /// the way round plus round end caps, rather than an aliased radial cut.
    fn arc(
        &mut self,
        center: (f32, f32),
        radius: f32,
        half_width: f32,
        start: f32,
        end: f32,
        peak: u16,
    ) {
        // An empty or non-finite sweep draws nothing — and keeps the `clamp`
        // below, which panics on a reversed or `NaN` bound, well fed.
        if !start.is_finite() || !end.is_finite() || end < start {
            return;
        }
        let pad = half_width + FEATHER;
        let xs = span(center.0 - radius - pad, center.0 + radius + pad, self.width);
        let ys = span(
            center.1 - radius - pad,
            center.1 + radius + pad,
            self.height,
        );
        for y in ys {
            for x in xs.clone() {
                let (px, py) = (fx(x), fx(y));
                let (run, rise) = (px - center.0, py - center.1);
                // Cheap radial reject before the trig: the distance to the full
                // circle is a lower bound on the distance to any arc of it.
                let radial = (run.hypot(rise) - radius).abs();
                if radial > pad {
                    continue;
                }
                // The dial's own angle: measured from 12 o'clock, clockwise.
                let theta = run.atan2(-rise).clamp(start, end);
                let near = polar(center, radius, theta);
                let distance = (px - near.0).hypot(py - near.1);
                let intensity = shade(coverage(distance, half_width), peak);
                if intensity > 0 {
                    self.raise(x, y, intensity);
                }
            }
        }
    }
}

/// The point `radius` from `origin` at dial angle `theta` — measured from 12
/// o'clock and growing clockwise, so `0.0` is straight up and `+π/2` is due
/// right. A negative `radius` points the other way (how the counterweight is
/// placed behind the pivot).
fn polar(origin: (f32, f32), radius: f32, theta: f32) -> (f32, f32) {
    (
        origin.0 + radius * theta.sin(),
        origin.1 - radius * theta.cos(),
    )
}

/// How much of a pixel a shape covers, given the perpendicular `distance` from
/// it and the shape's solid `half_width`: full inside, ramping linearly to
/// nothing over [`FEATHER`] beyond. A non-finite distance covers nothing.
fn coverage(distance: f32, half_width: f32) -> f32 {
    if distance <= half_width {
        return 1.0;
    }
    // `max` returns the other operand for a `NaN`, so a non-finite distance
    // lands on `0.0` rather than propagating.
    (1.0 - (distance - half_width) / FEATHER).max(0.0)
}

/// Scale `peak` by a `0.0..=1.0` coverage into an intensity.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn shade(coverage: f32, peak: u16) -> u16 {
    // The clamped product is finite and within `0.0..=255.0` (`peak` never
    // exceeds 255), so the truncating cast is exact and never wraps; a `NaN`
    // coverage clamps to the low end and reads as unlit.
    (coverage.clamp(0.0, 1.0) * f32::from(peak)).round() as u16
}

/// A small buffer coordinate or dimension as an exact `f32`. Buffer sizes are
/// far below `u16::MAX`, and `u16 → f32` is lossless, so this needs no lossy
/// cast at all.
fn fx(value: usize) -> f32 {
    f32::from(u16::try_from(value).unwrap_or(u16::MAX))
}

/// The most subdivisions per division a scale arc of `arc_len` logical pixels
/// can carry with adjacent ticks still clearing [`MIN_TICK_SPACING`] (#931).
///
/// At least `1`: a face too small even for the division boundaries still draws
/// them, because a scale with no marks at all is not a scale.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn tick_budget(arc_len: f32, divisions: usize) -> usize {
    let budget = arc_len / (MIN_TICK_SPACING * fx(divisions.max(1)));
    if !budget.is_finite() {
        return 1;
    }
    // Clamped into `1.0..=u16::MAX` before the cast, so the truncation is exact
    // and never wraps.
    budget.clamp(1.0, f32::from(u16::MAX)).floor() as usize
}

/// The widest bloom radius a face with this arc radius may spend, in logical
/// pixels — [`BLOOM_ARC_DIV`]-th of the arc, never below `1` (#931).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn bloom_cap(radius: f32) -> usize {
    let cap = radius / BLOOM_ARC_DIV;
    if !cap.is_finite() {
        return 1;
    }
    // Same clamp-then-cast contract as [`tick_budget`].
    cap.clamp(1.0, f32::from(u16::MAX)).floor() as usize
}

/// The pixel range covering `[low, high]` on one axis, clipped to `0..dim`.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn span(low: f32, high: f32, dim: usize) -> std::ops::Range<usize> {
    // Both ends are clamped into `0.0..=dim` before the cast, so the truncation
    // is exact and never wraps; a non-finite end saturates to `0` and yields an
    // empty range.
    let start = low.floor().clamp(0.0, fx(dim)) as usize;
    let end = (high.ceil() + 1.0).clamp(0.0, fx(dim)) as usize;
    start..end.max(start)
}

/// Clamp a deflection to the dial's mechanical travel: the needle may swing
/// [`OVERTRAVEL`] past either end of the scale — a real pointer bangs its stop
/// on a full-scale slam — but never off the face. A non-finite deflection reads
/// as the low end.
fn on_dial(fraction: f32) -> f32 {
    if fraction.is_finite() {
        fraction.clamp(-OVERTRAVEL, 1.0 + OVERTRAVEL)
    } else {
        0.0
    }
}

/// Where the needle was `back` seconds ago, extrapolated from its current
/// velocity — one motion-blur sample.
///
/// First-order, which is plenty over the ~50 ms shutter, and **exact at rest**:
/// with zero velocity every sample equals the current deflection, so the whole
/// smear collapses onto the blade and the max-combine erases it.
fn trail_fraction(fraction: f32, velocity: f32, back: f32) -> f32 {
    fraction - velocity * back
}

#[cfg(test)]
mod tests {
    use super::{
        ARC_HW, BLADE_TIP, DEFAULT_DAMPING, DEFAULT_FREQ_HZ, DisplayStyle, FEATHER, Gauge, Grid,
        MAJOR_HW, MAX_DAMPING, MAX_FREQ_HZ, MID_LEN_BONUS, MIN_DAMPING, MIN_FREQ_HZ, Needle,
        OVERTRAVEL, Tick, VALUE_HW_BONUS, coverage, fx, on_dial, polar, shade, span,
        trail_fraction,
    };
    use std::f32::consts::PI;

    /// A 60 Hz frame, the tick rate the physics tests read in.
    const FRAME: f32 = 1.0 / 60.0;

    /// Run a needle for `secs` at `dt`, returning every deflection sampled.
    fn trajectory(needle: &mut Needle, secs: f32, dt: f32) -> Vec<f32> {
        let mut out = vec![needle.fraction()];
        let mut elapsed = 0.0;
        while elapsed < secs {
            needle.advance(dt);
            elapsed += dt;
            out.push(needle.fraction());
        }
        out
    }

    /// The default needle, stepped from rest to mid-scale — comfortably clear of
    /// the dial's stops, so the overshoot is the physics' and nothing else's.
    fn stepped() -> Needle {
        let mut needle = Needle::new();
        needle.set_target(0.5);
        needle
    }

    /// Seconds for an `(hz, ζ)` spring's *slowest* mode to decay by about
    /// `e⁻¹²` — a generous settling budget to hold a test to.
    ///
    /// Underdamped, that mode is the envelope `exp(-ζωt)`. **Over**damped it is
    /// not: the response splits into two real modes and the slow one,
    /// `ω(ζ - √(ζ²-1))`, is far lazier than `ζω` — at ζ = 4 it is 30× lazier.
    /// Budgeting from `ζω` there would fail a needle that is simply creeping,
    /// which is what heavy damping is.
    fn settle_budget(hz: f32, zeta: f32) -> f32 {
        let omega = 2.0 * PI * hz;
        let rate = if zeta > 1.0 {
            omega * (zeta - (zeta * zeta - 1.0).sqrt())
        } else {
            zeta * omega
        };
        12.0 / rate
    }

    // ── The spring is a spring ───────────────────────────────────────────────

    /// **The assertion a lerp cannot pass.** A step change overshoots: some
    /// sample of the response exceeds the target, and the trajectory is
    /// therefore not monotone. Both halves are checked against a reference lerp
    /// in the same test, so "not a lerp" is a property this suite verifies
    /// rather than a claim in a PR body.
    #[test]
    fn a_lerp_cannot_overshoot_but_the_spring_does() {
        const TARGET: f32 = 0.5;

        // A textbook lerp — `x += (target - x) * k`, the thing #397 ruled out.
        // It is monotone by construction: each step moves a fixed fraction of
        // the remaining gap, so it approaches from one side and never arrives
        // from the other.
        let mut lerp = 0.0_f32;
        let mut lerp_peak = 0.0_f32;
        let mut lerp_monotone = true;
        for _ in 0..600 {
            let previous = lerp;
            lerp += (TARGET - lerp) * 0.2;
            lerp_monotone &= lerp >= previous;
            lerp_peak = lerp_peak.max(lerp);
        }
        assert!(
            lerp_peak <= TARGET,
            "a lerp never exceeds its target (peaked at {lerp_peak})"
        );
        assert!(lerp_monotone, "a lerp only ever approaches from one side");

        // The spring does both of the things the lerp cannot.
        let path = trajectory(&mut stepped(), 3.0, FRAME);
        let peak = path.iter().copied().fold(f32::MIN, f32::max);
        assert!(
            peak > TARGET + 0.05,
            "the needle overshoots its target: peaked at {peak}, target {TARGET}"
        );
        assert!(
            path.windows(2).any(|w| w[1] < w[0]),
            "and comes back down — the response is not monotone"
        );
    }

    /// The overshoot is not merely present, it is the *right size*: the first
    /// peak of a second-order step response sits `exp(-πζ/√(1-ζ²))` above the
    /// target, at `t = π/ω√(1-ζ²)`. Checking both pins the damping ratio and
    /// the natural frequency as observable behaviour, not just fields.
    #[test]
    fn the_step_response_matches_the_textbook_second_order_peak() {
        let zeta = DEFAULT_DAMPING;
        let omega = 2.0 * PI * DEFAULT_FREQ_HZ;
        let damped = omega * (1.0 - zeta * zeta).sqrt();
        let want_overshoot = (-PI * zeta / (1.0 - zeta * zeta).sqrt()).exp();
        let want_peak_at = PI / damped;

        // A fine step so the sampled peak lands close to the true one.
        let dt = 1.0 / 2000.0;
        let path = trajectory(&mut stepped(), 2.0, dt);
        let (index, peak) =
            path.iter().enumerate().fold(
                (0, f32::MIN),
                |best, (i, &v)| {
                    if v > best.1 { (i, v) } else { best }
                },
            );

        let overshoot = (peak - 0.5) / 0.5;
        assert!(
            (overshoot - want_overshoot).abs() < 0.01,
            "first overshoot {overshoot} ≈ the textbook {want_overshoot}"
        );
        assert!(
            (want_overshoot - 0.163).abs() < 0.001,
            "and ζ = 0.5 is the documented 16.3%, not something else"
        );
        let peak_at = dt * super::fx(index);
        assert!(
            (peak_at - want_peak_at).abs() < 0.01,
            "the peak lands at {peak_at}s ≈ the textbook π/ω_d = {want_peak_at}s"
        );
    }

    /// A second bounce follows the first overshoot, on the other side of the
    /// target — "overshoot **and settle**", not a single kick. At ζ = 0.5 the
    /// second excursion is the first one squared (2.7%), so it is small but
    /// real.
    #[test]
    fn the_needle_bounces_back_through_the_target_before_settling() {
        let path = trajectory(&mut stepped(), 3.0, 1.0 / 2000.0);
        let peak = path.iter().copied().fold(f32::MIN, f32::max);
        let peak_at = path.iter().position(|&v| v >= peak).expect("a peak");
        let after = &path[peak_at..];
        let trough = after.iter().copied().fold(f32::MAX, f32::min);
        assert!(
            trough < 0.5,
            "it swings back past the target on the rebound (to {trough})"
        );
        let rebound = after
            .iter()
            .skip_while(|&&v| v > trough)
            .copied()
            .fold(f32::MIN, f32::max);
        assert!(
            rebound > 0.5,
            "and crosses back once more — a bounce, not a single kick ({rebound})"
        );
        assert!(
            rebound - 0.5 < peak - 0.5,
            "each excursion is smaller than the last"
        );
    }

    // ── It settles ───────────────────────────────────────────────────────────

    /// Within a bounded number of ticks the needle is within epsilon of its
    /// target — **and stays there**. Checked across the whole supported spring
    /// range, so no configuration leaves a needle ringing forever.
    #[test]
    fn it_settles_within_a_bounded_number_of_ticks_and_stays() {
        for hz in [MIN_FREQ_HZ, 0.5, DEFAULT_FREQ_HZ, 8.0, MAX_FREQ_HZ] {
            for zeta in [MIN_DAMPING, 0.3, DEFAULT_DAMPING, 1.0, 2.0, MAX_DAMPING] {
                let mut needle = Needle::new().frequency(hz).damping(zeta);
                needle.set_target(0.75);
                let budget = settle_budget(hz, zeta);
                let mut elapsed = 0.0;
                while elapsed < budget {
                    needle.advance(FRAME);
                    elapsed += FRAME;
                }
                assert!(
                    (needle.fraction() - 0.75).abs() < 0.01,
                    "hz={hz} ζ={zeta}: arrived in {elapsed}s (at {})",
                    needle.fraction()
                );
                assert!(needle.is_settled(), "hz={hz} ζ={zeta}: and is at rest");
                // …and stays: another two budgets of ticks move it nowhere.
                while elapsed < budget * 3.0 {
                    needle.advance(FRAME);
                    elapsed += FRAME;
                    assert!(
                        (needle.fraction() - 0.75).abs() < 0.01,
                        "hz={hz} ζ={zeta}: stays settled"
                    );
                }
            }
        }
    }

    /// `is_settled` is honest at both ends: false while the needle is still
    /// swinging, true once it has arrived.
    #[test]
    fn is_settled_reports_arrival() {
        let mut needle = stepped();
        assert!(!needle.is_settled(), "a fresh step has not arrived");
        needle.advance(0.1);
        assert!(!needle.is_settled(), "mid-swing is not settled");
        for _ in 0..200 {
            needle.advance(FRAME);
        }
        assert!(needle.is_settled(), "and eventually it is");
    }

    // ── Frame-rate independence ──────────────────────────────────────────────

    /// The same wall-clock duration produces the same trajectory at different
    /// tick rates. This is the property a per-tick spring does **not** have: a
    /// fixed-step integrator run at 15 Hz reaches a visibly different place than
    /// one run at 240 Hz over the same second.
    #[test]
    fn two_tick_rates_produce_the_same_trajectory() {
        // Sample every 1/15 s — a common instant for every rate below, each an
        // exact multiple of it (30, 60, 120 and 240 Hz).
        for multiple in [2u16, 4, 8, 16] {
            let rate = 15.0 * f32::from(multiple);
            let mut fast = stepped();
            let mut slow = stepped();
            let mut worst = 0.0_f32;
            for _ in 0..30 {
                slow.advance(1.0 / 15.0);
                for _ in 0..multiple {
                    fast.advance(1.0 / rate);
                }
                worst = worst.max((fast.fraction() - slow.fraction()).abs());
            }
            assert!(
                worst < 1.0e-3,
                "{rate} Hz vs 15 Hz diverge by {worst} over 2 s of wall clock"
            );
        }
    }

    /// The stronger statement behind that: the closed-form step **composes**.
    /// Ten 100 ms steps land where one 1 s step does, because each is the exact
    /// solution over its interval rather than an approximation of it.
    #[test]
    fn the_closed_form_step_composes_exactly() {
        for zeta in [MIN_DAMPING, DEFAULT_DAMPING, 1.0, 2.5] {
            let mut split = Needle::new().damping(zeta);
            let mut whole = Needle::new().damping(zeta);
            split.set_target(0.6);
            whole.set_target(0.6);
            for _ in 0..10 {
                split.advance(0.1);
            }
            whole.advance(1.0);
            assert!(
                (split.fraction() - whole.fraction()).abs() < 1.0e-5,
                "ζ={zeta}: 10×100ms = {} vs 1×1s = {}",
                split.fraction(),
                whole.fraction()
            );
            assert!(
                (split.velocity() - whole.velocity()).abs() < 1.0e-4,
                "ζ={zeta}: and so do the velocities"
            );
        }
    }

    // ── Stability ────────────────────────────────────────────────────────────

    /// No divergence anywhere in the supported envelope — including the stiffest
    /// spring at absurd timesteps, which is exactly where a numerically
    /// integrated one blows up. Every branch's envelope is `exp(-ζωt)`, so the
    /// state can only contract.
    #[test]
    fn the_step_never_diverges_at_any_supported_rate() {
        for hz in [MIN_FREQ_HZ, DEFAULT_FREQ_HZ, MAX_FREQ_HZ] {
            for zeta in [MIN_DAMPING, DEFAULT_DAMPING, 1.0, MAX_DAMPING] {
                for dt in [1.0e-6_f32, 1.0 / 240.0, FRAME, 1.0 / 15.0, 0.5, 1.0, 10.0] {
                    let mut needle = Needle::new().frequency(hz).damping(zeta);
                    needle.set_target(1.0);
                    let budget = settle_budget(hz, zeta);
                    // At least 400 steps whatever the rate — a huge timestep is
                    // exactly where a numerically integrated spring explodes —
                    // and at most 20 000, so a microsecond step stays quick.
                    let (mut elapsed, mut steps) = (0.0_f32, 0u32);
                    while (elapsed < budget || steps < 400) && steps < 20_000 {
                        needle.advance(dt);
                        elapsed += dt;
                        steps += 1;
                        assert!(
                            needle.fraction().is_finite() && needle.velocity().is_finite(),
                            "hz={hz} ζ={zeta} dt={dt}: state stayed finite"
                        );
                        // A pointer can overshoot, but never by more than the
                        // step itself — nothing here amplifies.
                        assert!(
                            needle.fraction().abs() < 2.0,
                            "hz={hz} ζ={zeta} dt={dt}: bounded ({})",
                            needle.fraction()
                        );
                    }
                    // Only meaningful once enough wall clock has actually been
                    // simulated; a 1 µs step capped at 20 000 covers 20 ms.
                    if elapsed >= budget {
                        assert!(
                            (needle.fraction() - 1.0).abs() < 0.01,
                            "hz={hz} ζ={zeta} dt={dt}: and converges"
                        );
                    }
                }
            }
        }
    }

    /// The one real guard: a negative `dt` would run the closed form backwards
    /// and *grow* the envelope. It, and every non-finite `dt`, is a no-op.
    #[test]
    fn nonsense_timesteps_are_no_ops() {
        let mut needle = stepped();
        needle.advance(0.1);
        let before = (needle.fraction(), needle.velocity());
        for dt in [
            0.0,
            -0.0,
            -1.0,
            -1.0e9,
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
        ] {
            needle.advance(dt);
            assert!(
                (needle.fraction() - before.0).abs() < 1.0e-9
                    && (needle.velocity() - before.1).abs() < 1.0e-9,
                "dt={dt} moved the needle"
            );
        }
    }

    // ── Configuration ────────────────────────────────────────────────────────

    /// Damping is the knob that controls overshoot, and it does: less damping
    /// overshoots more, critical damping does not overshoot at all, and an
    /// overdamped needle creeps.
    #[test]
    fn damping_controls_the_overshoot() {
        let peak_for = |zeta: f32| {
            let mut needle = Needle::new().damping(zeta);
            needle.set_target(0.5);
            trajectory(&mut needle, 4.0, 1.0 / 2000.0)
                .iter()
                .copied()
                .fold(f32::MIN, f32::max)
        };
        let (light, default, critical, heavy) =
            (peak_for(0.2), peak_for(0.5), peak_for(1.0), peak_for(3.0));
        assert!(light > default, "less damping overshoots more");
        assert!(default > 0.5 + 0.05, "the default overshoots visibly");
        assert!(
            critical - 0.5 < 1.0e-3,
            "critical damping barely overshoots ({critical})"
        );
        assert!(heavy <= critical + 1.0e-6, "and overdamped never does");
    }

    /// Frequency is the knob that controls *speed*, independently of overshoot:
    /// a faster spring reaches the same relative peak sooner.
    #[test]
    fn frequency_controls_the_speed_not_the_shape() {
        let peak_at = |hz: f32| {
            let mut needle = Needle::new().frequency(hz);
            needle.set_target(0.5);
            let dt = 1.0 / 4000.0;
            let path = trajectory(&mut needle, 4.0, dt);
            let best = path.iter().copied().fold(f32::MIN, f32::max);
            let at = path.iter().position(|&v| v >= best).unwrap_or(0);
            (dt * super::fx(at), best)
        };
        let (slow_at, slow_peak) = peak_at(0.5);
        let (fast_at, fast_peak) = peak_at(4.0);
        assert!(fast_at < slow_at, "a stiffer spring arrives sooner");
        assert!(
            (fast_peak - slow_peak).abs() < 0.01,
            "but overshoots by the same fraction — that is ζ's job"
        );
    }

    /// Out-of-band spring parameters clamp rather than producing a needle that
    /// never settles (ζ = 0) or never moves (0 Hz), and non-finite ones are
    /// ignored outright.
    #[test]
    fn spring_parameters_clamp_and_reject_nonsense() {
        let clamped = Needle::new().frequency(1.0e6).damping(1.0e6);
        assert!((clamped.omega - 2.0 * PI * MAX_FREQ_HZ).abs() < 1.0e-3);
        assert!((clamped.zeta - MAX_DAMPING).abs() < 1.0e-6);
        let floored = Needle::new().frequency(-5.0).damping(0.0);
        assert!((floored.omega - 2.0 * PI * MIN_FREQ_HZ).abs() < 1.0e-4);
        assert!((floored.zeta - MIN_DAMPING).abs() < 1.0e-6);
        let base = Needle::new();
        let kept = Needle::new().frequency(f32::NAN).damping(f32::INFINITY);
        assert!((kept.omega - base.omega).abs() < 1.0e-6, "NaN Hz ignored");
        assert!((kept.zeta - base.zeta).abs() < 1.0e-6, "inf ζ ignored");
    }

    /// The caller's range maps both ways, clamps out-of-range targets, ignores
    /// non-finite ones, and rejects a degenerate range instead of dividing by
    /// zero.
    #[test]
    fn the_range_maps_values_and_rejects_nonsense() {
        let mut needle = Needle::new().range(0.0, 100.0);
        needle.set_target(75.0);
        assert!((needle.target() - 75.0).abs() < 1.0e-4);
        needle.settle();
        assert!((needle.value() - 75.0).abs() < 1.0e-4, "value maps back");
        assert!((needle.fraction() - 0.75).abs() < 1.0e-6, "physics is unit");

        needle.set_target(1.0e6);
        assert!(
            (needle.target() - 100.0).abs() < 1.0e-3,
            "clamps to full scale"
        );
        needle.set_target(-50.0);
        assert!(needle.target().abs() < 1.0e-3, "and to zero");
        let before = needle.target();
        needle.set_target(f32::NAN);
        assert!((needle.target() - before).abs() < 1.0e-6, "NaN ignored");

        // A degenerate or non-finite range is refused; the previous one stands.
        let degenerate = Needle::new().range(5.0, 5.0).range(9.0, 1.0);
        assert!((degenerate.low).abs() < 1.0e-6 && (degenerate.high - 1.0).abs() < 1.0e-6);
        let nonsense = Needle::new().range(f32::NAN, 1.0).range(0.0, f32::INFINITY);
        assert!((nonsense.high - 1.0).abs() < 1.0e-6);

        // A shifted, negative-origin range works the same way.
        let mut celsius = Needle::new().range(-20.0, 40.0);
        celsius.set_target(10.0);
        celsius.settle();
        assert!((celsius.fraction() - 0.5).abs() < 1.0e-6, "mid-scale");
        assert!((celsius.value() - 10.0).abs() < 1.0e-4);
    }

    /// `settle` (#422) parks the needle on its reading in one call — the hide
    /// edge — while keeping the spring and range a rebuilt `Needle::new()` would
    /// have thrown away.
    #[test]
    fn settle_parks_the_needle_but_keeps_the_configuration() {
        let mut needle = Needle::new().frequency(5.0).damping(0.25).range(0.0, 200.0);
        needle.set_target(150.0);
        needle.advance(0.05);
        assert!(!needle.is_settled(), "mid-swing");

        needle.settle();
        assert!(
            (needle.value() - 150.0).abs() < 1.0e-3,
            "parked on the reading"
        );
        assert!(needle.velocity().abs() < 1.0e-9, "and stopped dead");
        assert!(needle.is_settled());
        assert!(
            (needle.omega - 2.0 * PI * 5.0).abs() < 1.0e-3,
            "spring kept"
        );
        assert!((needle.zeta - 0.25).abs() < 1.0e-6, "damping kept");
        assert!((needle.high - 200.0).abs() < 1.0e-6, "range kept");

        // And the next real change still animates from there.
        needle.set_target(50.0);
        needle.advance(0.05);
        assert!(!needle.is_settled(), "a fresh target still swings");
    }

    // ── Motion blur ──────────────────────────────────────────────────────────

    /// The smear is sampled in *time*, so it is frame-rate independent like the
    /// physics — and it collapses to nothing at rest, which is why a settled
    /// gauge shows no trail.
    #[test]
    fn the_motion_blur_samples_time_and_vanishes_at_rest() {
        for back in [0.0, 0.0125, 0.05] {
            assert!(
                (trail_fraction(0.4, 0.0, back) - 0.4).abs() < 1.0e-9,
                "a still needle's samples all coincide with it"
            );
        }
        // Moving, the samples trail behind by velocity × time.
        assert!((trail_fraction(0.4, 2.0, 0.05) - 0.3).abs() < 1.0e-6);
        assert!(
            (trail_fraction(0.4, -2.0, 0.05) - 0.5).abs() < 1.0e-6,
            "both ways"
        );
    }

    /// …and the rendered consequence: a swinging needle smears (more of the face
    /// is lit) while a settled one at the same reading does not.
    #[test]
    fn a_moving_needle_smears_and_a_settled_one_does_not() {
        let style = DisplayStyle::Oled;
        let lit = |gauge: &Gauge| {
            let bg = style.palette().bg;
            gauge
                .render(style)
                .data()
                .chunks_exact(4)
                .filter(|px| px[..4] != bg[..])
                .count()
        };

        let mut parked = Gauge::new();
        parked.set_target(0.5);
        parked.settle();

        // Same reading, but caught mid-swing on the way through.
        let mut swinging = Gauge::new();
        swinging.set_target(1.0);
        while swinging.fraction() < 0.5 {
            swinging.advance(1.0 / 240.0);
        }
        assert!(
            swinging.needle().velocity() > 0.5,
            "the sampled needle really is moving"
        );
        assert!(
            (swinging.fraction() - parked.fraction()).abs() < 0.02,
            "and reads about the same, so the trail is the only difference"
        );
        assert!(
            lit(&swinging) > lit(&parked),
            "the moving needle lights more of the face: {} vs {}",
            lit(&swinging),
            lit(&parked)
        );
    }

    // ── The face ─────────────────────────────────────────────────────────────

    /// The needle actually points where the reading says — checked as an
    /// *angle*, against the dial's own mapping.
    ///
    /// The measurement window is a radius band strictly inside the tick marks
    /// and outside the hub, which only the blade can reach: the ticks stop at
    /// `radius - major_len`, the value arc and its bloom well outside that, so
    /// every lit pixel in the band belongs to the pointer.
    #[test]
    fn the_needle_points_where_the_reading_says() {
        let style = DisplayStyle::Oled;
        let pointing_at = |fraction: f32| {
            let mut gauge = Gauge::with_size(144, 64).scale(1);
            gauge.set_target(fraction);
            gauge.settle();
            let dial = gauge.dial();
            let frame = gauge.render(style);
            let bg = style.palette().bg;
            let (inner, outer) = (dial.tip * 0.45, dial.tip * 0.90);
            let (mut sum, mut count) = (0.0_f32, 0usize);
            for y in 0..frame.height() {
                for x in 0..frame.width() {
                    let px = frame.get(i32::try_from(x).unwrap(), i32::try_from(y).unwrap());
                    if px.is_none_or(|p| p[..4] == bg[..]) {
                        continue;
                    }
                    let (run, rise) = (super::fx(x) - dial.pivot.0, super::fx(y) - dial.pivot.1);
                    let radius = run.hypot(rise);
                    if radius >= inner && radius <= outer {
                        sum += run.atan2(-rise);
                        count += 1;
                    }
                }
            }
            assert!(count > 0, "the blade lit something at fraction {fraction}");
            sum / super::fx(count)
        };
        let dial = Gauge::with_size(144, 64).scale(1).dial();
        // Within 3° of the angle the dial says that reading maps to.
        let slack = 3.0_f32.to_radians();
        for fraction in [0.0_f32, 0.25, 0.5, 0.75, 1.0] {
            let want = dial.angle(fraction);
            let got = pointing_at(fraction);
            assert!(
                (got - want).abs() < slack,
                "at {fraction} the needle points {}°, the dial says {}°",
                got.to_degrees(),
                want.to_degrees()
            );
        }
        // …and it sweeps monotonically rather than jumping about.
        let sweep: Vec<f32> = (0..=8u8).map(|i| pointing_at(f32::from(i) / 8.0)).collect();
        assert!(
            sweep.windows(2).all(|w| w[1] > w[0]),
            "the needle sweeps monotonically: {sweep:?}"
        );
    }

    /// The value arc fills to the reading: a higher reading lights strictly more
    /// of the scale.
    #[test]
    fn the_value_arc_fills_to_the_reading() {
        let style = DisplayStyle::Oled;
        let filled = |fraction: f32| {
            let mut gauge = Gauge::new();
            gauge.set_target(fraction);
            gauge.settle();
            let frame = gauge.render(style);
            let bg = style.palette().bg;
            // Count only the top band, where the arc runs and the hub cannot
            // reach; the needle contributes the same handful of pixels either way.
            let rows = frame.height() / 3;
            let mut lit = 0usize;
            for y in 0..rows {
                for x in 0..frame.width() {
                    let px = frame.get(i32::try_from(x).unwrap(), i32::try_from(y).unwrap());
                    if px.is_some_and(|p| p[..4] != bg[..]) {
                        lit += 1;
                    }
                }
            }
            lit
        };
        let (empty, half, full) = (filled(0.0), filled(0.5), filled(1.0));
        assert!(
            half > empty,
            "half scale lights more than none: {half} > {empty}"
        );
        assert!(full > half, "and full lights more still: {full} > {half}");
    }

    /// The flat dial furniture is a stable reference like the scope's graticule:
    /// it is redrawn from the field every frame, so it neither decays nor
    /// flickers however long the needle swings past it.
    ///
    /// Probed at the *top* division's tick, with the needle confined to the
    /// bottom quarter of the scale — the value arc only ever fills to the
    /// reading, so this is a face pixel nothing in the lit layer can reach.
    #[test]
    fn the_dial_face_never_decays_or_flickers() {
        let style = DisplayStyle::Oled;
        let mut gauge = Gauge::with_size(144, 64).scale(1);
        gauge.settle();
        let dial = gauge.dial();
        let probe_at = super::polar(
            dial.pivot,
            dial.radius - dial.major_len * 0.5,
            dial.angle(1.0),
        );
        #[allow(clippy::cast_possible_truncation)]
        let (px, py) = (probe_at.0.round() as i32, probe_at.1.round() as i32);
        let probe = |g: &Gauge| g.render(style).get(px, py);

        let baseline = probe(&gauge);
        assert!(
            baseline.is_some_and(|c| c[..4] != style.palette().bg[..]),
            "the face paints a visible tick pixel"
        );
        // Swing the needle back and forth across the low end of the scale.
        for round in 0..6 {
            gauge.set_target(if round % 2 == 0 { 0.25 } else { 0.0 });
            for _ in 0..40 {
                gauge.advance(1.0 / 60.0);
                assert_eq!(probe(&gauge), baseline, "the face pixel never changes");
            }
        }
    }

    /// The tick layout is what it says: one tick per subdivision, majors on the
    /// division boundaries, exactly one mid-scale mark, and degenerate counts
    /// clamp instead of dividing by zero.
    ///
    /// At the **default** face, where the resolved subdivision count is the
    /// configured one — `the_default_face_takes_none_of_the_small_dial_rules`
    /// is what pins that, and `a_small_dial_thins_its_scale_rather_than_
    /// merging_the_ticks` is the other side of it.
    #[test]
    fn the_tick_layout_follows_the_divisions() {
        let gauge = Gauge::new();
        let marks: Vec<(f32, Tick)> = gauge.tick_marks(gauge.dial()).collect();
        assert_eq!(marks.len(), 4 * 5 + 1, "inclusive of both ends");
        assert!(marks[0].0.abs() < 1.0e-6 && (marks[marks.len() - 1].0 - 1.0).abs() < 1.0e-6);
        assert_eq!(
            marks.iter().filter(|m| m.1 == Tick::Mid).count(),
            1,
            "exactly one mid-scale mark"
        );
        assert_eq!(
            marks.iter().filter(|m| m.1 != Tick::Minor).count(),
            5,
            "five division boundaries for four divisions"
        );
        // Degenerate counts clamp to a single interval rather than panicking.
        let bare_gauge = Gauge::new().ticks(0, 0);
        let bare: Vec<(f32, Tick)> = bare_gauge.tick_marks(bare_gauge.dial()).collect();
        assert_eq!(bare.len(), 2, "the two ends");
    }

    // ── Sizing / host invariant ──────────────────────────────────────────────

    /// The rendered buffer follows the scale hint, fits the sidebar card, and
    /// clamps degenerate sizes/scales rather than producing a broken buffer.
    #[test]
    fn buffer_dimensions_follow_the_scale_hint() {
        let gauge = Gauge::new();
        let frame = gauge.render(DisplayStyle::Vfd);
        assert_eq!(
            (frame.width(), frame.height()),
            (gauge.width(), gauge.height())
        );
        assert_eq!(
            frame.width(),
            288,
            "the default fits the ~296 px sidebar card"
        );
        assert!(frame.width() <= 296);
        let scaled = Gauge::with_size(20, 10).scale(3);
        assert_eq!((scaled.width(), scaled.height()), (60, 30));
        assert_eq!(scaled.render(DisplayStyle::Vfd).width(), 60);
        // Degenerate sizes/scales clamp to at least 1, never a zero buffer.
        let tiny = Gauge::with_size(0, 0).scale(0);
        let frame = tiny.render(DisplayStyle::Oled);
        assert_eq!((frame.width(), frame.height()), (1, 1));
        assert_eq!(frame.data().len(), frame.width() * frame.height() * 4);
    }

    /// The host invariant across skins, sizes, sweeps and readings — including
    /// the ones that peg the needle against its stops — and the frame is a
    /// screen: fully opaque, `len == w * h * 4`, always.
    #[test]
    fn every_render_satisfies_the_host_invariant() {
        for style in DisplayStyle::ALL {
            for (width, height) in [(1, 1), (7, 3), (64, 32), (144, 64), (240, 40)] {
                for target in [-9.0_f32, 0.0, 0.5, 1.0, 9.0] {
                    let mut gauge = Gauge::with_size(width, height).sweep_deg(f32::NAN);
                    gauge.set_target(target);
                    let frame = gauge.tick(1.0 / 60.0, style);
                    assert_eq!(
                        frame.data().len(),
                        frame.width() * frame.height() * 4,
                        "{style:?} {width}x{height} target={target}"
                    );
                    assert!(frame.width() > 0 && frame.height() > 0);
                    assert!(
                        frame.data().chunks_exact(4).all(|px| px[3] == 0xff),
                        "{style:?} {width}x{height}: the gauge is a screen, wall to wall"
                    );
                }
            }
        }
    }

    /// The whole supported sweep range renders a valid, opaque face — including
    /// the narrowest and widest, and a non-finite one (which keeps the default).
    #[test]
    fn every_sweep_renders_a_valid_face() {
        for degrees in [-90.0_f32, 10.0, 90.0, 150.0, 180.0, 900.0, f32::NAN] {
            let mut gauge = Gauge::new().sweep_deg(degrees);
            gauge.set_target(1.0);
            gauge.settle();
            let frame = gauge.render(DisplayStyle::Vfd);
            assert_eq!(frame.data().len(), frame.width() * frame.height() * 4);
            assert!(
                frame.data().chunks_exact(4).all(|px| px[3] == 0xff),
                "{degrees}"
            );
        }
    }

    /// Renders are deterministic and the three skins render differently.
    #[test]
    fn render_is_deterministic_and_skins_differ() {
        let mut a = Gauge::with_size(96, 48);
        let mut b = Gauge::with_size(96, 48);
        a.set_target(0.65);
        b.set_target(0.65);
        for _ in 0..20 {
            a.advance(1.0 / 60.0);
            b.advance(1.0 / 60.0);
        }
        assert_eq!(
            a.render(DisplayStyle::Vfd),
            b.render(DisplayStyle::Vfd),
            "same inputs, same bytes"
        );
        let (vfd, lcd, oled) = (
            a.render(DisplayStyle::Vfd),
            a.render(DisplayStyle::Lcd),
            a.render(DisplayStyle::Oled),
        );
        assert_ne!(vfd, lcd);
        assert_ne!(vfd, oled);
        assert_ne!(lcd, oled);
    }

    // ── Drawing primitives ───────────────────────────────────────────────────

    /// The soft edge: full coverage inside the solid half-width, a linear ramp
    /// to nothing over `FEATHER` beyond it, and nothing at all for a non-finite
    /// distance.
    #[test]
    fn coverage_is_solid_then_ramps_to_nothing() {
        assert!((coverage(0.0, 1.0) - 1.0).abs() < 1.0e-6);
        assert!(
            (coverage(1.0, 1.0) - 1.0).abs() < 1.0e-6,
            "solid to the edge"
        );
        let mid = coverage(1.0 + super::FEATHER / 2.0, 1.0);
        assert!(
            (mid - 0.5).abs() < 1.0e-6,
            "half-way out is half-lit ({mid})"
        );
        assert!(
            coverage(1.0 + super::FEATHER, 1.0).abs() < 1.0e-6,
            "and out"
        );
        assert!(coverage(99.0, 1.0).abs() < 1.0e-6);
        assert!(coverage(f32::NAN, 1.0).abs() < 1.0e-6, "NaN covers nothing");
        // Monotone non-increasing in distance.
        let mut previous = 1.0;
        for step in 0..40u8 {
            let now = coverage(f32::from(step) / 10.0, 1.0);
            assert!(now <= previous + 1.0e-6);
            previous = now;
        }
    }

    /// `shade` maps coverage onto an intensity, saturating at both ends.
    #[test]
    fn shade_maps_coverage_onto_intensity() {
        assert_eq!(shade(1.0, 255), 255);
        assert_eq!(shade(0.0, 255), 0);
        assert_eq!(shade(0.5, 200), 100);
        assert_eq!(shade(9.0, 255), 255, "over-unit clamps");
        assert_eq!(shade(-9.0, 255), 0, "and under");
        assert_eq!(shade(f32::NAN, 255), 0, "NaN reads as unlit");
    }

    /// Spans clip to the buffer and never wrap, whatever they are handed.
    #[test]
    fn spans_clip_to_the_buffer() {
        assert_eq!(span(2.0, 4.0, 10), 2..5, "covers columns 2, 3 and 4");
        assert_eq!(span(-99.0, 99.0, 10), 0..10, "clips both ends");
        assert_eq!(span(20.0, 30.0, 10), 10..10, "entirely past the end");
        assert!(span(f32::NAN, f32::NAN, 10).is_empty());
        assert!(
            span(5.0, 1.0, 10).is_empty(),
            "reversed is empty, not a wrap"
        );
    }

    /// A zero-length segment is a disc — how the hub is drawn — soft-edged and
    /// clipped like everything else.
    #[test]
    fn a_zero_length_segment_is_a_disc() {
        let mut grid = Grid::new(11, 11);
        grid.segment((5.0, 5.0), (5.0, 5.0), 2.0, 2.0, 255);
        assert_eq!(grid.get(5, 5), 255, "solid at the center");
        assert_eq!(grid.get(7, 5), 255, "solid to the radius");
        assert!(grid.get(8, 5) > 0 && grid.get(8, 5) < 255, "then feathers");
        assert_eq!(grid.get(10, 5), 0, "and is out well before the edge");
        // Radially symmetric.
        assert_eq!(grid.get(5, 7), grid.get(3, 5));
        assert_eq!(grid.get(5, 3), grid.get(7, 5));
        // A disc straddling the edge clips silently rather than wrapping.
        let mut edge = Grid::new(6, 6);
        edge.segment((-3.0, -3.0), (-3.0, -3.0), 2.0, 2.0, 255);
        assert!(
            edge.v.iter().all(|&v| v == 0),
            "fully outside draws nothing"
        );
    }

    /// Shapes **max**-combine: an overlapping dimmer shape never adds to a
    /// brighter one (which is what would fatten a settled needle's edge into its
    /// own motion blur).
    #[test]
    fn overlapping_shapes_take_the_brighter_not_the_sum() {
        let mut grid = Grid::new(9, 9);
        grid.segment((4.0, 4.0), (4.0, 4.0), 2.0, 2.0, 200);
        grid.segment((4.0, 4.0), (4.0, 4.0), 2.0, 2.0, 100);
        assert_eq!(grid.get(4, 4), 200, "the brighter stamp stands");
        grid.segment((4.0, 4.0), (4.0, 4.0), 2.0, 2.0, 255);
        assert_eq!(grid.get(4, 4), 255, "and a brighter one still wins");
    }

    /// The dial's mechanical stops bound the drawn angle without ever bounding
    /// the physics.
    #[test]
    fn the_dial_stops_bound_the_drawn_angle_only() {
        assert!((on_dial(0.5) - 0.5).abs() < 1.0e-6, "in range, untouched");
        assert!(
            (on_dial(9.0) - (1.0 + OVERTRAVEL)).abs() < 1.0e-6,
            "high stop"
        );
        assert!((on_dial(-9.0) + OVERTRAVEL).abs() < 1.0e-6, "low stop");
        assert!(on_dial(f32::NAN).abs() < 1.0e-6, "NaN parks at zero");

        // A full-scale slam really does drive the needle past full scale — the
        // physics is not clamped, only the drawing.
        let mut needle = Needle::new();
        needle.set_target(1.0);
        let peak = trajectory(&mut needle, 2.0, 1.0 / 2000.0)
            .iter()
            .copied()
            .fold(f32::MIN, f32::max);
        assert!(peak > 1.0 + OVERTRAVEL, "the pointer overshoots the stop");
        assert!(
            (on_dial(peak) - (1.0 + OVERTRAVEL)).abs() < 1.0e-6,
            "and the face parks it against the stop"
        );
    }

    // ── Small dials (#931) ───────────────────────────────────────────────────

    /// The bounding box of everything that is not the skin's field, as
    /// `(left, top, right, bottom)` in buffer pixels — the *drawn* extent, as
    /// opposed to the extent the geometry says it should have.
    ///
    /// `None` when the frame is entirely field. Not meaningful on
    /// [`DisplayStyle::Crt`], whose comb and vignette tint every pixel on the
    /// glass; the callers below use the three flat skins.
    fn ink_bounds(
        frame: &super::Frame,
        style: DisplayStyle,
    ) -> Option<(usize, usize, usize, usize)> {
        let bg = style.palette().bg;
        let mut bounds: Option<(usize, usize, usize, usize)> = None;
        for y in 0..frame.height() {
            for x in 0..frame.width() {
                let px = frame.get(i32::try_from(x).unwrap(), i32::try_from(y).unwrap());
                if px.is_none_or(|p| p[..4] == bg[..]) {
                    continue;
                }
                bounds = Some(match bounds {
                    None => (x, y, x, y),
                    Some((l, t, r, b)) => (l.min(x), t.min(y), r.max(x), b.max(y)),
                });
            }
        }
        bounds
    }

    /// **The byte-identity gate for #931.** The default 144×64 face takes *none*
    /// of the small-dial rules: every one of them is a floor, a cap or a
    /// re-centring the default sits clear of, which is what makes
    /// `tests/single_ink_golden.rs`'s `gauge` row — captured on `origin/main`
    /// before any of this landed — still green.
    ///
    /// One assertion per rule, each naming the margin it has, so a later change
    /// to any threshold that would reach the default fails **here**, with the
    /// number, rather than as an opaque digest mismatch in another file.
    #[test]
    fn the_default_face_takes_none_of_the_small_dial_rules() {
        let gauge = Gauge::new();
        let dial = gauge.dial();

        // (a) The height binds, so the pivot stays *seated* — the centring in
        //     `dial()` is not even reachable at the default aspect.
        let by_height = dial.pivot.1 - super::EDGE;
        let by_width = (dial.pivot.0 - super::EDGE) / dial.half.sin();
        assert!(
            by_height < by_width,
            "the default face fits the arc by height ({by_height:.2}), not by width \
             ({by_width:.2}) — if that ever inverts the face silently re-centres"
        );
        assert!(
            (dial.pivot.1 - 54.04).abs() < 0.01 && (dial.radius - 50.54).abs() < 0.01,
            "the seated pivot and radius are what they always were: {dial:?}"
        );

        // (b) Every configured subdivision survives the spacing floor.
        assert_eq!(dial.subdivisions, super::DEFAULT_SUBDIVISIONS);
        let spacing = dial.radius * 2.0 * dial.half / fx(4 * super::DEFAULT_SUBDIVISIONS);
        assert!(
            spacing > super::MIN_TICK_SPACING,
            "default tick spacing {spacing:.2} px clears the {:.2} px floor",
            super::MIN_TICK_SPACING
        );

        // (c) The counterweight is well past the minimum worth drawing …
        assert!(
            dial.tail > super::MIN_TAIL,
            "default counterweight {:.2} px clears the {:.2} px minimum",
            dial.tail,
            super::MIN_TAIL
        );

        // (d) … the tick floors are inert (the fractions are what set them) …
        assert!(
            (dial.major_len - dial.radius * super::MAJOR_LEN_FRAC).abs() < 1.0e-4
                && (dial.minor_len - dial.radius * super::MINOR_LEN_FRAC).abs() < 1.0e-4,
            "tick lengths still come from the radius, not from a floor: {dial:?}"
        );

        // (e) … and so is the blade floor.
        assert!(
            (dial.blade - dial.radius * super::BLADE_FRAC).abs() < 1.0e-4,
            "the blade half-width still comes from the radius: {:.3}",
            dial.blade
        );

        // (f) The bloom cap is at least as wide as the widest halo any skin
        //     asks for, so `render` caps nothing at the default.
        let cap = super::bloom_cap(dial.radius);
        for style in DisplayStyle::ALL {
            if let Some(bloom) = style.palette().bloom {
                assert!(
                    cap >= bloom.radius,
                    "{}'s bloom radius {} must survive the default face's cap of {cap}",
                    style.name(),
                    bloom.radius
                );
            }
        }
    }

    /// A **square** dial is centred in its buffer and stays inside it — the two
    /// properties a 48×48 or 64×64 face needs and the seated wide face never
    /// did. Both are measured on the rendered pixels, not on the geometry that
    /// produced them.
    ///
    /// `Crt` is excluded because its comb and vignette tint the whole glass, so
    /// "not field" stops meaning "ink" there; the three flat skins carry the
    /// same face.
    ///
    /// The **containment** half is checked at three readings; the **centring**
    /// half only at rest, because the lit value arc fills from the low end and
    /// is drawn 0.35 px wider than the scale arc it covers, so any non-zero
    /// reading is legitimately a third of a pixel wider on the left.
    #[test]
    fn a_small_square_dial_is_centred_and_fits_its_buffer() {
        for edge in [48usize, 64] {
            for style in [DisplayStyle::Vfd, DisplayStyle::Lcd, DisplayStyle::Oled] {
                let last = edge - 1;
                let face = |fraction: f32| {
                    let mut gauge = Gauge::with_size(edge, edge).scale(1);
                    gauge.set_target(fraction);
                    gauge.settle();
                    let frame = gauge.render(style);
                    ink_bounds(&frame, style).expect("a square dial draws something")
                };

                // Nothing is clipped, at either stop or mid-scale: the outermost
                // ring of pixels is field.
                for fraction in [0.0_f32, 0.5, 1.0] {
                    let (left, top, right, bottom) = face(fraction);
                    assert!(
                        top > 0 && left > 0 && right < last && bottom < last,
                        "{edge}×{edge} {} at {fraction}: ink runs to the buffer edge \
                         (l={left} t={top} r={right} b={bottom} of {last})",
                        style.name()
                    );
                }

                let (left, top, right, bottom) = face(0.0);
                // Horizontally the pivot is the buffer's centre by construction,
                // so with the value arc empty the two margins match to the pixel.
                assert_eq!(
                    left,
                    last - right,
                    "{edge}×{edge} {}: the face is not horizontally centred",
                    style.name()
                );
                // Vertically it is the centring in `dial()` that has to hold it,
                // and the feather rounds the two ends off differently, so allow
                // two pixels — an *un*-centred face is out by ten or more.
                let (above, below) = (top, last - bottom);
                assert!(
                    above.abs_diff(below) <= 2,
                    "{edge}×{edge} {}: face not vertically centred — {above} px above, \
                     {below} px below",
                    style.name()
                );
            }
        }
    }

    /// A small dial thins its **scale** rather than letting adjacent ticks merge
    /// into a band: the configured subdivisions are pulled down until the marks
    /// clear [`MIN_TICK_SPACING`], and never pushed up.
    #[test]
    fn a_small_dial_thins_its_scale_rather_than_merging_the_ticks() {
        for (cols, rows, want) in [(144usize, 64usize, 5usize), (64, 64, 3), (48, 48, 2)] {
            let gauge = Gauge::with_size(cols, rows);
            let dial = gauge.dial();
            assert_eq!(
                dial.subdivisions, want,
                "{cols}×{rows} draws {want} subdivisions per division"
            );
            let spacing = dial.radius * 2.0 * dial.half / fx(4 * dial.subdivisions);
            assert!(
                spacing >= super::MIN_TICK_SPACING,
                "{cols}×{rows}: resolved spacing {spacing:.2} px clears the floor"
            );
            // …and the marks the face draws are the resolved count, not the
            // configured one.
            let minors = gauge
                .tick_marks(dial)
                .filter(|m| m.1 == Tick::Minor)
                .count();
            assert_eq!(
                minors,
                4 * (want - 1),
                "{cols}×{rows}: {minors} minor ticks on the face"
            );
        }
        // The floor only ever *reduces*: a face that asks for fewer than it
        // could carry keeps its own count.
        let sparse = Gauge::with_size(48, 48).ticks(4, 1);
        assert_eq!(sparse.dial().subdivisions, 1);
    }

    /// A small dial drops the **counterweight** and thins the **needle**: below
    /// [`MIN_TAIL`] the stub is inside the hub's own feathered disc, and the
    /// blade's floor relaxes to [`BLADE_TIP`] so the pointer stays a needle
    /// instead of becoming a wedge.
    #[test]
    fn a_small_dial_drops_the_counterweight_and_thins_the_needle() {
        let default = Gauge::new().dial();
        let sixty_four = Gauge::with_size(64, 64).dial();
        let forty_eight = Gauge::with_size(48, 48).dial();

        assert!(
            default.tail > 0.0 && sixty_four.tail > 0.0,
            "both have room"
        );
        assert!(
            forty_eight.tail == 0.0,
            "48×48's {:.2} px stub is under the {:.2} px minimum",
            forty_eight.radius * super::TAIL_FRAC,
            super::MIN_TAIL
        );
        // The blade thins with the dial rather than parking on the old 0.8 px
        // floor — and the default's is untouched by the relaxation.
        assert!(
            forty_eight.blade < 0.8 && forty_eight.blade >= BLADE_TIP,
            "48×48 blade half-width {:.3} px",
            forty_eight.blade
        );
        assert!(sixty_four.blade > 0.8 && default.blade > 0.8);

        // And the pixels agree: with the needle straight up, the wedge behind
        // the pivot is lit at 64 and dark at 48.
        let behind_the_pivot = |edge: usize| {
            let mut gauge = Gauge::with_size(edge, edge).scale(1);
            gauge.set_target(0.5);
            gauge.settle();
            let dial = gauge.dial();
            let frame = gauge.render(DisplayStyle::Lcd);
            let probe = dial.pivot.1 + dial.hub + FEATHER + 1.5;
            #[allow(clippy::cast_possible_truncation)]
            let (px, py) = (dial.pivot.0.round() as i32, probe.round() as i32);
            frame
                .get(px, py)
                .is_some_and(|p| p[..4] != DisplayStyle::Lcd.palette().bg[..])
        };
        assert!(
            behind_the_pivot(64),
            "64×64 keeps its counterweight, so the pixel behind the hub is lit"
        );
        assert!(
            !behind_the_pivot(48),
            "48×48 drops it, so the same pixel is field"
        );
    }

    /// The needle reaches both mechanical stops on a small dial: at `0.0` and
    /// `1.0` it lands on the ends of the sweep the face draws, on **every flat
    /// skin** and at three buffer shapes.
    ///
    /// # The annulus has to exclude the face's furniture
    ///
    /// The obvious probe — average the angle of every lit pixel in an annulus
    /// around the blade — is only honest if the annulus really does contain the
    /// blade and nothing else. `tip * 0.45 .. tip * 0.90` does *not*: the mid
    /// tick at 12 o'clock is drawn `major_len * MID_LEN_BONUS` inward from the
    /// arc and then rounded off by a `MAJOR_HW + FEATHER` end cap, and that cap
    /// is a **fixed pixel cost** while the gap it has to clear is a fraction of
    /// the radius. On the default face it clears by 3.9 px; at 48×48 it does not
    /// clear at all, and two faint tick pixels sitting at 0° join an average of
    /// twenty-six blade pixels sitting at −75°.
    ///
    /// Measured, low stop, `scale(1)`, with the naive annulus and with this one:
    ///
    /// | face | skin | naive | furniture excluded |
    /// |---|---|---|---|
    /// | 48×48 | Lcd | **+5.51°** | +0.32° |
    /// | 48×48 | Vfd / Oled | +2.82° | −0.23° |
    /// | 64×64 | any flat | −0.15° | −0.22° |
    /// | 32×64 | Lcd | **+53.59°** | −0.98° |
    /// | 144×64 | any flat | −0.09° | −0.12° |
    ///
    /// So the needle was never short: the metric was reading the dial's own
    /// centre mark. The bloomed skins hid it, because their halo multiplies the
    /// blade's pixel count and dilutes the two tick pixels — which is exactly
    /// why rendering one skin was not enough. `Lcd` has no bloom at all and is
    /// the skin that showed it.
    ///
    /// The slack is **2°**, tightened from the 3° this test shipped with: the
    /// worst measurement above is 0.98°, at the extreme 32×64 aspect, and the
    /// two tuned sizes are inside 0.4°. `Crt` is left out for
    /// `a_small_square_dial_is_centred_and_fits_its_buffer`'s reason — its comb
    /// and vignette tint every pixel on the glass, so "not field" stops meaning
    /// "ink" — though it measures identically to `Vfd` today.
    #[test]
    fn the_needle_reaches_both_stops_on_a_small_dial() {
        let slack = 2.0_f32.to_radians();
        for (cols, rows) in [(48usize, 48usize), (64, 64), (32, 64)] {
            for style in [DisplayStyle::Vfd, DisplayStyle::Lcd, DisplayStyle::Oled] {
                let mut gauge = Gauge::with_size(cols, rows).scale(1);
                let dial = gauge.dial();
                // Outside the hub, and strictly inside the innermost ink the
                // *face* can reach: the mid tick's end cap, widened by the halo.
                let inner = dial.tip * 0.45;
                let outer = (dial.tip * 0.90).min(
                    dial.radius
                        - dial.major_len * MID_LEN_BONUS
                        - MAJOR_HW
                        - FEATHER
                        - fx(super::bloom_cap(dial.radius)),
                );
                assert!(
                    outer > inner,
                    "{cols}×{rows}: no room between the hub ({inner:.2}) and the furniture \
                     ({outer:.2}) to measure the blade in"
                );
                for fraction in [0.0_f32, 1.0] {
                    gauge.set_target(fraction);
                    gauge.settle();
                    let frame = gauge.render(style);
                    let bg = style.palette().bg;
                    let (mut sum, mut count) = (0.0_f32, 0usize);
                    for y in 0..frame.height() {
                        for x in 0..frame.width() {
                            let px =
                                frame.get(i32::try_from(x).unwrap(), i32::try_from(y).unwrap());
                            if px.is_none_or(|p| p[..4] == bg[..]) {
                                continue;
                            }
                            let (run, rise) = (fx(x) - dial.pivot.0, fx(y) - dial.pivot.1);
                            let radius = run.hypot(rise);
                            if radius >= inner && radius <= outer {
                                sum += run.atan2(-rise);
                                count += 1;
                            }
                        }
                    }
                    assert!(
                        count > 0,
                        "{cols}×{rows} {}: the blade lit something at {fraction}",
                        style.name()
                    );
                    let got = sum / fx(count);
                    let want = dial.angle(fraction);
                    assert!(
                        (got - want).abs() < slack,
                        "{cols}×{rows} {} at {fraction}: the needle points {:.2}°, the sweep \
                         end is {:.2}° ({count} px sampled)",
                        style.name(),
                        got.to_degrees(),
                        want.to_degrees()
                    );
                }
            }
        }
    }

    /// A **centred** face leaves room for its own halo at both ends (#931).
    ///
    /// `render` blooms the lit layer *after* `dial()` has placed the pivot, so a
    /// budget counting only the ink can centre a face and then have its glow cut
    /// off — and cut off asymmetrically, on the side the centring moved toward.
    /// It does not show at 48×48 or 64×64, which have ten px of margin against a
    /// one px halo; it shows on a **near-square** face like `105×64`, where the
    /// arc fits by width by a hair and the ink budget leaves 0.40 px against a
    /// halo of 3.
    ///
    /// The invariant, checked across fourteen buffer shapes: **a face is centred
    /// only when the halo fits at both ends**, and otherwise it stays seated —
    /// the pre-#931 placement, which cannot be a regression. The two tuned sizes
    /// are asserted to still be centred, so the fallback cannot quietly swallow
    /// them.
    ///
    /// Verified against `origin/main` by digesting every render at eleven buffer
    /// shapes × four skins × three readings: `105×64` and `110×66` come back
    /// **byte-identical** to the pre-#931 tree, and `100×62` differs on `Crt`
    /// alone — that one is the proportional halo cap doing its documented job
    /// (arc radius 47.6 ⇒ cap 2, and `Crt` is the only skin asking for 3), not a
    /// placement change. `96×48` and `64×32` are seated because their *height*
    /// binds, so centring was never on the table for them; they are in the list
    /// so a later change to which axis wins is caught here.
    #[test]
    fn a_centred_face_leaves_room_for_its_own_halo() {
        let mut centred_seen = 0usize;
        for (cols, rows) in [
            (144usize, 64usize),
            (110, 66),
            (105, 64),
            (100, 62),
            (96, 96),
            (96, 48),
            (72, 72),
            (64, 64),
            (64, 32),
            (48, 64),
            (48, 48),
            (48, 32),
            (32, 64),
            (24, 24),
        ] {
            let dial = Gauge::with_size(cols, rows).dial();
            let halo = fx(super::bloom_cap(dial.radius));
            let ink_above = dial.radius + ARC_HW + VALUE_HW_BONUS + FEATHER;
            let ink_below = FEATHER
                + if dial.tail > 0.0 {
                    dial.tail + dial.blade * super::TAIL_FLARE
                } else {
                    dial.hub
                };
            if !dial.centred {
                continue;
            }
            centred_seen += 1;
            let top = dial.pivot.1 - ink_above;
            let bottom = (fx(rows) - 1.0) - (dial.pivot.1 + ink_below);
            assert!(
                top >= halo - 1.0e-4,
                "{cols}×{rows} is centred with only {top:.2} px above its arc, and its halo \
                 is {halo}"
            );
            assert!(
                bottom >= halo - 1.0e-4,
                "{cols}×{rows} is centred with only {bottom:.2} px below its hub, and its \
                 halo is {halo}"
            );
        }
        assert!(centred_seen >= 8, "the sweep exercises the centred branch");
        for (cols, rows) in [(48usize, 48usize), (64, 64)] {
            assert!(
                Gauge::with_size(cols, rows).dial().centred,
                "{cols}×{rows} is a tuned size and must still centre"
            );
        }
        // …and the four aspects whose renders were digested against `origin/main`
        // all stay seated: the two near-square ones because the halo does not
        // fit, the two wide ones because their height binds in the first place.
        for (cols, rows) in [(105usize, 64usize), (100, 62), (96, 48), (64, 32)] {
            assert!(
                !Gauge::with_size(cols, rows).dial().centred,
                "{cols}×{rows} must keep the seated placement it had before #931"
            );
        }
    }

    /// The **bloom** stays a fixed fraction of the dial instead of a fixed
    /// pixel count, so a small face's halo does not swallow its needle.
    ///
    /// Both halves matter: the cap arithmetic, and that `render` actually
    /// applies it. The second is measured the way #930 measured the blur — the
    /// lit cross-section straight through the settled blade — because that is
    /// the only place the capped radius is observable from outside.
    #[test]
    fn the_bloom_never_outgrows_a_small_dial() {
        // A sixteenth of the arc, floored at one, and never `0` or a wrap.
        assert_eq!(super::bloom_cap(50.54), 3, "the default face caps nothing");
        assert_eq!(super::bloom_cap(28.99), 1, "a 64×64 dial");
        assert_eq!(super::bloom_cap(20.71), 1, "a 48×48 dial");
        assert_eq!(super::bloom_cap(0.5), 1, "floored, never zero");
        assert_eq!(super::bloom_cap(f32::NAN), 1, "and non-finite is the floor");

        // The needle at 80 %, on `Vfd` (the only default-skin bloom the small
        // faces still carry), measured along the row through the blade's
        // mid-span. Uncapped, `Vfd`'s radius-2 halo would add 4 logical px to
        // this; the cap holds it to 2.
        let mut gauge = Gauge::with_size(48, 48).scale(1);
        gauge.set_target(0.8);
        gauge.settle();
        let dial = gauge.dial();
        let frame = gauge.render(DisplayStyle::Vfd);
        let bg = DisplayStyle::Vfd.palette().bg;
        let mid = polar(dial.pivot, dial.tip * 0.6, dial.angle(0.8));
        #[allow(clippy::cast_possible_truncation)]
        let (row, col) = (mid.1.round() as i32, mid.0.round() as i32);
        // A ±6 px window about the blade: at this radius and angle the scale
        // arc crosses ~13 px further out on each side and the nearest tick is
        // seven rows up, so nothing but the pointer and its halo is in here.
        // Measured: **9** px with the cap, **12** without it (the row cuts the
        // 45° blade diagonally, so each logical px of halo costs √2 here).
        let lit = (col - 6..=col + 6)
            .filter(|&x| frame.get(x, row).is_some_and(|p| p[..4] != bg[..]))
            .count();
        assert!(
            lit <= 10,
            "a 48×48 Vfd blade lights {lit} px across its own row — the cap is what holds \
             that under the 12 px an uncapped halo draws"
        );
    }
}
