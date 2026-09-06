//! **The single-ink regression guard for the colour axis (#857).**
//!
//! #857 generalised the kit's one shared composite path — the loop in
//! `Emission::composite` that drives the lit layer toward the palette ink —
//! so a surface can resolve a *different* ink per pixel. Every other widget in
//! the kit still takes the single-ink path, and the contract of that change is
//! that they take it **unchanged, down to the byte**.
//!
//! Argument-shaped tests can't prove that: `composite` now delegates to
//! `composite_with`, so asserting the two agree is a tautology the compiler
//! already enforces. What can prove it is a golden: the exact bytes each
//! public single-ink entry point produces, for a fixed input, in every skin.
//! Those bytes are a function of the whole path — the ghost pass, the
//! [`Emission`] stamp, the box-blur bloom, the CRT mask's comb and vignette,
//! the `mix` rounding, and the palette itself — so **any** change to any of
//! them moves a hash here.
//!
//! The values below were captured from `main` at `5e50ad7`, i.e. from the
//! pre-#857 tree, by running this same file against it. They are deliberately
//! *not* regenerated as part of this suite: a golden that regenerates itself
//! asserts nothing.
//!
//! # When one of these goes red
//!
//! It means a kit render changed. That is sometimes correct (a deliberate
//! palette or falloff change) and sometimes the bug this file exists to catch
//! (a "refactor" of the shared path that silently altered every widget). Do
//! not update the number until you know which — the failure message prints the
//! observed value so the update is mechanical *once the change is intended*.
//!
//! An integration test rather than a unit one on purpose: it goes through the
//! crate's public API only, so it stays honest about what a consumer sees and
//! cannot accidentally reach past a boundary to reconstruct the expected
//! bytes.
//!
//! # Recaptured by #930
//!
//! The **two gauge rows** ([`gauge_single_ink_bytes_are_unchanged`] and
//! [`gauge_midswing_single_ink_bytes_are_unchanged`]) no longer hold the
//! values their own doc comments describe capturing. They were recaptured on
//! branch `fix/930-gauge-bloom-b`, based on `origin/main` at `1755239`, by
//! running this file with `gauge.rs`'s `BLOOM_RADIUS_DIV` at its shipped `2` —
//! the taste change #930 settled: **the gauge wears half the bloom radius its
//! skin asks for**, ceilinged by #931's proportional `bloom_cap`. Nothing else
//! moved with it: `FEATHER` is still `1.15`, `TRAIL_T` still
//! `[150, 104, 66, 34]`, `TRAIL_SPAN_SECS` still `0.05`, `BLOOM_ARC_DIV` still
//! `16.0`, and no skin palette in `style.rs` was touched. Every non-gauge row
//! in this file is untouched, which is itself the check that #930 stayed
//! inside the gauge.
//!
//! **Two of the four values in each gauge row are byte-identical to the
//! pre-#930 capture, and that identity is the regression check worth stating.**
//! `Lcd` (index 1) carries `bloom: None` and never blooms at all; `Oled`
//! (index 2) asks for radius `1`, which `div_ceil(2)` leaves at `1`. Only
//! `Vfd` (2 → 1) and `Crt` (3 → 2) may move under #930, and exactly those two
//! did, in both rows. A future diff that reddens the `Lcd` or `Oled` cell of a
//! gauge row is therefore *not* a bloom-taste change — it is a change to the
//! shared composite path this file exists to catch.
//!
//! Recapturing rather than loosening is deliberate. The value of a golden is
//! that a deliberate taste change arrives as a reviewed diff of hashes with a
//! commit named beside it, not as a widened tolerance.

use hytte_preem::{DisplayStyle, Frame, Gauge, LedStrip, Marquee, TextBox, dot_matrix, seven_seg};

/// FNV-1a over the frame's RGBA bytes plus its dimensions, so a buffer that
/// changed *shape* fails as loudly as one that changed *colour*.
///
/// A hash rather than a checked-in byte blob because these buffers run to tens
/// of kilobytes each and 20 of them would swamp the repository; FNV-1a because
/// it is four lines of `std`, deterministic across platforms and rustc
/// versions (no `DefaultHasher`, whose output is explicitly not stable), and
/// nothing here is adversarial.
fn digest(frame: &Frame) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |b: u8| {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    };
    for b in u32::try_from(frame.width())
        .unwrap_or(u32::MAX)
        .to_le_bytes()
    {
        eat(b);
    }
    for b in u32::try_from(frame.height())
        .unwrap_or(u32::MAX)
        .to_le_bytes()
    {
        eat(b);
    }
    for &b in frame.data() {
        eat(b);
    }
    h
}

/// Every skin, in `DisplayStyle::ALL` order, so a golden row lines up with the
/// enum.
fn skins() -> [DisplayStyle; 4] {
    DisplayStyle::ALL
}

/// Assert one widget's four per-skin digests, naming the skin and printing the
/// observed value on a mismatch.
fn check(widget: &str, expected: [u64; 4], render: impl Fn(DisplayStyle) -> Frame) {
    for (style, want) in skins().into_iter().zip(expected) {
        let got = digest(&render(style));
        assert_eq!(
            got,
            want,
            "{widget} on {} changed: expected {want:#018x}, got {got:#018x} — the single-ink \
             composite path is supposed to be byte-identical (#857). See this file's header \
             before updating the constant.",
            style.name()
        );
    }
}

#[test]
fn dot_matrix_single_ink_bytes_are_unchanged() {
    check(
        "dot_matrix",
        [
            0x5e21_6e08_4e18_0683,
            0xb094_697d_2496_923d,
            0x6396_c6e5_c573_0c31,
            0x4d24_0d0e_b412_ae39,
        ],
        |style| dot_matrix("HELLO 42", style),
    );
}

#[test]
fn seven_seg_single_ink_bytes_are_unchanged() {
    check(
        "seven_seg",
        [
            0x57ad_6d30_e6e3_a872,
            0x65fa_6dfe_b2ad_2da7,
            0x2601_7433_8cea_5e39,
            0xd39b_97c6_47ad_59dd,
        ],
        |style| seven_seg("12:34", style),
    );
}

#[test]
fn led_strip_single_ink_bytes_are_unchanged() {
    check(
        "led_strip",
        [
            0xc3b3_5274_faac_52e4,
            0x216f_0d1b_3fb0_0464,
            0x5315_ef3c_248b_e12c,
            0xd0c2_498b_383c_a583,
        ],
        |style| LedStrip::new(style).leds(12).render(0.6, 0.8),
    );
}

#[test]
fn marquee_single_ink_bytes_are_unchanged() {
    check(
        "marquee",
        [
            0x626d_bc26_4cc8_8f41,
            0xb42e_a6c0_346a_cbb1,
            0x016f_b97b_9e18_71e5,
            0x7b45_e71a_4dbd_9fbb,
        ],
        |style| {
            Marquee::new(style)
                .window_px(120)
                .render("blinken lichten")
                .window(7)
        },
    );
}

/// The **default 144×64 needle gauge**, settled at 30 % of full scale.
///
/// Added by #931, which taught the face a set of small-dial rules — a centred
/// pivot when the arc fits by width, a subdivision floor, a counterweight
/// minimum, tick-length floors and a bloom cap proportional to the arc — every
/// one of which is a threshold the default face sits clear of. That claim is
/// exactly what a golden can settle and an argument cannot, and until #931 the
/// kit's most geometry-heavy widget was the one widget with no byte pin at all.
///
/// The four values were captured **before** any of #931's changes, by running
/// this test against `origin/main` at `d629f90`; they were then re-run against
/// the finished branch unchanged. So unlike the rows above they are not from
/// `5e50ad7`, but they are from a tree that predates the change they guard,
/// which is the property that matters.
///
/// Settled at a reading rather than at rest so the frame carries the whole
/// path: the flat face, the lit value arc, the tapered blade, the counterweight
/// and the hub, bloomed and composited.
///
/// **Recaptured by #930** (`BLOOM_RADIUS_DIV: 1 → 2` in `gauge.rs`, the gauge
/// halving its skin's bloom radius) — see this file's header, "Recaptured by
/// #930", for the provenance and for why exactly two of the four values below
/// moved.
#[test]
fn gauge_single_ink_bytes_are_unchanged() {
    check(
        "gauge",
        [
            0x5f60_bb60_9dc3_2a3a,
            0x471c_d528_d5fa_3612,
            0x3500_9832_2b70_3bea,
            0xfebf_540b_f11e_adca,
        ],
        |style| {
            let mut gauge = Gauge::new();
            gauge.set_target(0.30);
            gauge.settle();
            gauge.render(style)
        },
    );
}

/// A gauge settled at 30 %, retargeted to 80 %, then advanced six frames at
/// 60 Hz — the one state [`gauge_midswing_single_ink_bytes_are_unchanged`]
/// asserts on and renders. A shared helper so the assertions and the digests
/// `check` verifies always describe the same construction; building it twice,
/// by hand, in two places is exactly how a guard drifts from what it guards.
fn midswing() -> Gauge {
    let mut gauge = Gauge::new();
    gauge.set_target(0.30);
    gauge.settle();
    gauge.set_target(0.80);
    for _ in 0..6 {
        gauge.advance(1.0 / 60.0);
    }
    gauge
}

/// The **default 144×64 needle gauge**, mid-swing: settled at 30 %, retargeted
/// to 80 %, then advanced by six steps of `1.0 / 60.0` s (0.1 s of wall clock)
/// — see [`midswing`].
///
/// Added by #937. The row above pins the needle **settled** — [`Needle::advance`]
/// has not run since the last [`Needle::settle`], so its velocity is `0.0` and
/// `trail_fraction`'s `back` term (`fraction - velocity * back`) collapses to
/// the current fraction at every sample; the four `TRAIL_T` blades stack
/// exactly on the live blade and the max-combine in `Gauge::render` erases
/// the motion blur completely (documented in `render`'s own comment on why a
/// settled needle's blur "vanishes exactly"). A `TRAIL_T` retune, a change to
/// `TRAIL_SPAN_SECS`'s subdivision, or any other tweak to the blur fan is
/// therefore **invisible** to that row — it renders the one code path
/// (`velocity == 0.0`) where the trail loop runs but paints nothing.
///
/// This row closes that gap by advancing until the needle is actually
/// smearing. **Six** steps rather than one: a reviewed sweep of 1–12 steps
/// found trail coverage (the pixel difference between this render and a
/// `set_target(fraction); settle()` render at the same fraction, i.e. the
/// smear alone with the needle-moved delta subtracted out) peaks at six —
/// 1.8×–2.4× a single step's trail pixels, across all four skins — at 54 % of
/// the 30→80 swing, nowhere near the 80 % target and still well short of
/// [`Needle::is_settled`]. A change to `TRAIL_T`'s weights, its length, or
/// `TRAIL_SPAN_SECS` moves this digest; the settled row above is untouched by
/// definition, since velocity is still `0.0` there regardless of what the
/// trail path does with it. A change to the **static** face geometry
/// (`MIN_TICK_SPACING`, `MAJOR_HW`, the arc, the ticks, the hub, …) moves both
/// rows, because both render the same 144×64 face under a lit needle.
///
/// Two things worth knowing before touching this row on a red:
///
/// - **It also pins the spring**, not just the trail. Six steps of the
///   underdamped solver in [`Needle::advance`] exercise [`DEFAULT_FREQ_HZ`]
///   and [`DEFAULT_DAMPING`] in a way the settled row cannot ([`Needle::settle`]
///   snaps straight past the solver) — retuning either constant moves this
///   digest too. A red here is therefore not automatically a blur regression;
///   check what actually changed in `gauge.rs` before assuming a blur-taste
///   PR is the culprit.
/// - **It is the suite's first byte-exact pin on libm.** The underdamped
///   branch evaluates `(-decay * dt).exp()` and `(damped * dt).sin_cos()`,
///   neither of which `std` guarantees is correctly rounded, so — unlike every
///   other row in this file, which only ever reaches `sin`/`cos` at a constant
///   angle — this digest is sensitive to the platform libm. A toolchain/glibc
///   bump that reddens *only* this row with an otherwise clean diff is that,
///   not a kit bug; the fix is to recapture with fresh provenance (see below),
///   never to loosen the pin.
///
/// The step count is a **test fixture**, not the shell's frame step: the shell
/// hands the kit a real, µs-quantised `dt` —
/// `micros_to_secs(frame_time_us.saturating_sub(last))`, clamped at
/// `MAX_TICK_DT_US` (400 000 µs) in `trollshell/src/plugins/preem_render.rs`
/// — and an exact `1.0 / 60.0` never arrives except by coincidence. A kit
/// golden fixes `dt` by construction, which is the point; six repeats of that
/// fixture is not a claim about any real frame pacing.
///
/// The four values were captured by running this test against `origin/main`
/// at `d1a2d57` (this branch's own base — the row is new here, not guarding a
/// prior change, so a plain run against the base is the capture), and
/// **recaptured by #930** — see this file's header, "Recaptured by #930".
///
/// [`Needle::advance`]: hytte_preem::Needle::advance
/// [`Needle::settle`]: hytte_preem::Needle::settle
/// [`Needle::is_settled`]: hytte_preem::Needle::is_settled
/// [`DEFAULT_FREQ_HZ`]: hytte_preem::DEFAULT_FREQ_HZ
/// [`DEFAULT_DAMPING`]: hytte_preem::DEFAULT_DAMPING
#[test]
fn gauge_midswing_single_ink_bytes_are_unchanged() {
    let probe = midswing();
    // `is_settled()` is NOT the load-bearing guard here: it also reads false
    // at `advance(0.0)` (position 0.30 is still short of target 0.80), so on
    // its own it cannot catch a fixture that stopped actually moving.
    // Velocity is what the trail path reads (`trail_fraction`'s `back` term),
    // so it is the assertion this fixture actually depends on — and it is
    // NaN-safe the same way `is_settled` is: `NaN > 0.0` is false (this assert
    // fires), and `is_settled`'s `<=` pair is false on `NaN` too (the assert
    // below still fires).
    assert!(
        probe.needle().velocity().abs() > 0.0,
        "zero velocity would collapse the trail onto the live blade — see this test's doc comment"
    );
    assert!(
        !probe.is_settled(),
        "the probe settled within six frames — pick a state that is actually mid-swing"
    );

    check(
        "gauge midswing",
        [
            0x30fb_485c_3616_b2da,
            0x4738_7fc9_ee7d_8daa,
            0x34bc_7708_4385_c52a,
            0x3701_e7d1_5db7_bd3a,
        ],
        |style| midswing().render(style),
    );
}

#[test]
fn textbox_single_ink_bytes_are_unchanged() {
    check(
        "textbox",
        [
            0x6bdc_2dc2_3e5c_fe3f,
            0x3f45_99b5_b862_5387,
            0x99cf_361d_929f_5258,
            0x966e_224c_875a_dad1,
        ],
        |style| TextBox::styled(style).cols(10).render("preem 857"),
    );
}
