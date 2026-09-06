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
#[test]
fn gauge_single_ink_bytes_are_unchanged() {
    check(
        "gauge",
        [
            0xf28f_82e8_52ac_2312,
            0x471c_d528_d5fa_3612,
            0x3500_9832_2b70_3bea,
            0xf257_45aa_ec57_3372,
        ],
        |style| {
            let mut gauge = Gauge::new();
            gauge.set_target(0.30);
            gauge.settle();
            gauge.render(style)
        },
    );
}

/// The **default 144×64 needle gauge**, mid-swing: settled at 30 %, retargeted
/// to 80 %, then advanced by exactly one 60 Hz frame (`1.0 / 60.0` s).
///
/// Added by #937. The row above pins the needle **settled** — [`Needle::advance`]
/// has not run since the last [`Needle::settle`], so its velocity is `0.0` and
/// [`trail_fraction`]'s `back` term (`fraction - velocity * back`) collapses to
/// the current fraction at every sample; the four [`TRAIL_T`] blades stack
/// exactly on the live blade and the max-combine in [`Gauge::render`] erases
/// the motion blur completely (documented in `render`'s own comment on why a
/// settled needle's blur "vanishes exactly"). A `TRAIL_T` retune, a change to
/// `TRAIL_SPAN_SECS`'s subdivision, or any other tweak to the blur fan is
/// therefore **invisible** to that row — it renders the one code path
/// (`velocity == 0.0`) where the trail loop runs but paints nothing.
///
/// This row closes that gap: after one frame of real motion the needle carries
/// nonzero velocity, so each of the four trail samples lands at a distinct
/// fraction behind the live blade and the blur is actually on the buffer. A
/// change to `TRAIL_T`'s weights, its length, or `TRAIL_SPAN_SECS` moves this
/// digest; the settled row above is untouched by definition, since velocity is
/// still `0.0` there regardless of what the trail path does with it. A change
/// to the **static** face geometry (`MIN_TICK_SPACING`, `MAJOR_HW`, the arc,
/// the ticks, the hub, …) moves both rows, because both render the same 144×64
/// face under a lit needle.
///
/// The four values were captured by running this test against `origin/main` at
/// `d1a2d57`, i.e. before this row existed — a plain run against the branch's
/// own base, since the row is new here rather than guarding a prior change.
///
/// [`Needle::advance`]: hytte_preem::Needle::advance
/// [`Needle::settle`]: hytte_preem::Needle::settle
#[test]
fn gauge_midswing_single_ink_bytes_are_unchanged() {
    let mut probe = Gauge::new();
    probe.set_target(0.30);
    probe.settle();
    probe.set_target(0.80);
    probe.advance(1.0 / 60.0);
    assert!(
        !probe.is_settled(),
        "the probe settled within one frame — pick a state that is actually mid-swing"
    );
    assert!(
        probe.needle().velocity().abs() > 0.0,
        "zero velocity would collapse the trail onto the live blade — see this test's doc comment"
    );

    check(
        "gauge midswing",
        [
            0x9e38_425b_6403_800a,
            0xd271_62a7_df95_ec32,
            0x4faa_9cb1_a7cd_b84a,
            0x50b3_fbdf_55ef_de92,
        ],
        |style| {
            let mut gauge = Gauge::new();
            gauge.set_target(0.30);
            gauge.settle();
            gauge.set_target(0.80);
            gauge.advance(1.0 / 60.0);
            gauge.render(style)
        },
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
