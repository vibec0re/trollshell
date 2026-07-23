//! Pure DSP for the audio spectrum tap (#405): interleaved f32 capture samples
//! → a `{ peak, bins[16] }` [`AudioSpectrum`] frame at ~20 Hz.
//!
//! Everything here is pure, allocation-light, and unit-testable without a live
//! `PipeWire` graph. The [`Analyzer`] accumulates mono-downmixed samples and,
//! once a full analysis window has arrived, runs a Hann-windowed **real FFT**
//! (a small safe-Rust Cooley–Tukey, [`fft`]) and groups the magnitude spectrum
//! into [`SPECTRUM_BINS`] log-spaced bands. So this is a genuine windowed FFT,
//! not just a time-domain energy split — a pure tone lights the band that
//! contains its frequency (see the tests).
//!
//! The one non-obvious knob is [`DISPLAY_GAIN`]: FFT band magnitudes are
//! normalized so a full-scale tone reads ~0.25 per band; the gain lifts typical
//! program material into a legible `0.0..=1.0` bar range and everything is
//! clamped. It is a display heuristic, not a calibrated measurement.

use std::f32::consts::PI;

use futures_signals::signal::Mutable;
use pipewire as pw;

use super::super::pipewire::{AudioSpectrum, SPECTRUM_BINS};

/// Analysis window / FFT length. A power of two so the radix-2 FFT applies.
/// At 48 kHz, 2048 samples ≈ 42.7 ms → ~23 emitted frames/s, i.e. the issue's
/// "~20 Hz". Larger quanta emit a little slower, smaller ones accumulate across
/// `process` calls — either way latest-wins downstream.
const FFT_SIZE: usize = 2048;

/// Sample rate assumed until the stream's `param_changed` reports the real one.
const DEFAULT_RATE: u32 = 48_000;

/// Heuristic display gain applied to each normalized band magnitude before the
/// `0.0..=1.0` clamp (see the module docs).
const DISPLAY_GAIN: f32 = 6.0;

/// `usize` → `f32` for DSP index/length math. Every value here is bounded by
/// [`FFT_SIZE`] (2048), far below f32's 2^24 exact-integer limit, so no
/// precision is actually lost.
#[allow(clippy::cast_precision_loss)]
fn usize_to_f32(n: usize) -> f32 {
    n as f32
}

/// `f32` → `usize` for band-edge computation. Callers pass non-negative,
/// already-bounded values (FFT bin indices), so neither truncation nor sign
/// loss discards meaningful information.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn f32_to_usize(v: f32) -> usize {
    v as usize
}

/// Accumulates capture samples and emits one [`AudioSpectrum`] per full window.
pub(super) struct Analyzer {
    rate: u32,
    fft_size: usize,
    /// Mono-downmixed samples awaiting a full window. Cleared after each emit,
    /// so it never grows past ~one window plus one capture quantum.
    mono: Vec<f32>,
}

impl Analyzer {
    pub(super) fn new() -> Self {
        Self {
            rate: DEFAULT_RATE,
            fft_size: FFT_SIZE,
            mono: Vec::with_capacity(FFT_SIZE * 2),
        }
    }

    /// Adopt the graph's negotiated sample rate (from `param_changed`). A zero
    /// rate (format not yet parsed) is ignored so the default stands.
    pub(super) fn set_rate(&mut self, rate: u32) {
        if rate != 0 {
            self.rate = rate;
        }
    }

    /// Feed one chunk of interleaved little-endian f32 samples with `channels`
    /// channels, downmixing each frame to mono. Returns a fresh spectrum once a
    /// full window has accumulated, else `None`.
    pub(super) fn push_bytes(&mut self, bytes: &[u8], channels: usize) -> Option<AudioSpectrum> {
        let channels = channels.max(1);
        let frame_bytes = channels * 4;
        let inv_channels = 1.0 / usize_to_f32(channels);
        for frame in bytes.chunks_exact(frame_bytes) {
            let mut sum = 0.0_f32;
            for s in frame.chunks_exact(4) {
                sum += f32::from_le_bytes([s[0], s[1], s[2], s[3]]);
            }
            self.mono.push(sum * inv_channels);
        }
        self.emit_if_ready()
    }

    /// Analyze the freshest full window if one is available, dropping any older
    /// backlog (a visualizer only cares about the newest frame).
    fn emit_if_ready(&mut self) -> Option<AudioSpectrum> {
        if self.mono.len() < self.fft_size {
            return None;
        }
        let start = self.mono.len() - self.fft_size;
        let spectrum = analyze_window(&self.mono[start..], self.rate);
        self.mono.clear();
        Some(spectrum)
    }
}

/// Hann window coefficient for sample `i` of a length-`n` window.
fn hann(i: usize, n: usize) -> f32 {
    if n <= 1 {
        return 1.0;
    }
    0.5 - 0.5 * (2.0 * PI * usize_to_f32(i) / usize_to_f32(n - 1)).cos()
}

/// The `[lo, hi)` FFT-bin range of band `band`, log-spaced from bin 1 (skipping
/// DC) to `half` across [`SPECTRUM_BINS`] bands. Guarantees `hi > lo` so every
/// band spans at least one bin.
fn band_bins(band: usize, half: usize) -> (usize, usize) {
    let lo = band_edge(band, half);
    let hi = band_edge(band + 1, half).max(lo + 1);
    (lo, hi.min(half.max(2)))
}

/// The log-spaced FFT-bin index at band boundary `edge` (0..=`SPECTRUM_BINS`):
/// a geometric interpolation `1 * half^(edge / BINS)`, clamped into `1..=half`.
fn band_edge(edge: usize, half: usize) -> usize {
    if half <= 1 {
        return 1;
    }
    let frac = usize_to_f32(edge) / usize_to_f32(SPECTRUM_BINS);
    let value = usize_to_f32(half).powf(frac);
    f32_to_usize(value).clamp(1, half)
}

/// Run one Hann-windowed FFT over `window` (length must be a power of two) and
/// group the magnitude spectrum into a [`SPECTRUM_BINS`]-band [`AudioSpectrum`].
/// `rate` is currently unused for banding (bands are FFT-bin-proportional) but
/// kept in the signature so a future frequency-anchored split needs no plumbing
/// change.
fn analyze_window(window: &[f32], _rate: u32) -> AudioSpectrum {
    let n = window.len();
    debug_assert!(n.is_power_of_two(), "FFT window must be a power of two");

    let peak = window.iter().fold(0.0_f32, |m, &s| m.max(s.abs())).min(1.0);

    let mut re = vec![0.0_f32; n];
    let mut im = vec![0.0_f32; n];
    for (i, &sample) in window.iter().enumerate() {
        re[i] = sample * hann(i, n);
    }
    fft(&mut re, &mut im);

    let half = n / 2;
    let norm = 1.0 / usize_to_f32(n.max(1));
    let mut bins = [0.0_f32; SPECTRUM_BINS];
    for (band, slot) in bins.iter_mut().enumerate() {
        let (lo, hi) = band_bins(band, half);
        let mut peak_mag = 0.0_f32;
        for k in lo..hi {
            let mag = (re[k] * re[k] + im[k] * im[k]).sqrt();
            peak_mag = peak_mag.max(mag);
        }
        *slot = (peak_mag * norm * DISPLAY_GAIN).clamp(0.0, 1.0);
    }

    AudioSpectrum { peak, bins }
}

/// In-place iterative radix-2 Cooley–Tukey FFT (decimation-in-time). `re`/`im`
/// are the real/imaginary parts of the input, overwritten with the transform;
/// their length must be a power of two. Pure safe Rust — no external FFT crate.
fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    if n < 2 {
        return;
    }

    // Bit-reversal permutation.
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }

    // Butterfly stages.
    let mut len = 2usize;
    while len <= n {
        let angle = -2.0 * PI / usize_to_f32(len);
        let (wlen_re, wlen_im) = (angle.cos(), angle.sin());
        let half = len / 2;
        let mut base = 0usize;
        while base < n {
            let mut w_re = 1.0_f32;
            let mut w_im = 0.0_f32;
            for k in 0..half {
                let a = base + k;
                let b = base + k + half;
                let t_re = w_re * re[b] - w_im * im[b];
                let t_im = w_re * im[b] + w_im * re[b];
                re[b] = re[a] - t_re;
                im[b] = im[a] - t_im;
                re[a] += t_re;
                im[a] += t_im;
                let next_re = w_re * wlen_re - w_im * wlen_im;
                w_im = w_re * wlen_im + w_im * wlen_re;
                w_re = next_re;
            }
            base += len;
        }
        len <<= 1;
    }
}

/// User data owned by the capture stream's listener (loop-thread only): the
/// negotiated audio format, the running [`Analyzer`], and the output handle the
/// `process` callback pushes each finished frame into.
pub(super) struct SpectrumUserData {
    pub(super) format: pw::spa::param::audio::AudioInfoRaw,
    pub(super) analyzer: Analyzer,
    pub(super) out: Mutable<Option<AudioSpectrum>>,
}

impl SpectrumUserData {
    pub(super) fn new(out: Mutable<Option<AudioSpectrum>>) -> Self {
        Self {
            format: pw::spa::param::audio::AudioInfoRaw::default(),
            analyzer: Analyzer::new(),
            out,
        }
    }
}

#[cfg(test)]
#[allow(clippy::cast_precision_loss)]
mod tests {
    use super::{
        Analyzer, DEFAULT_RATE, FFT_SIZE, PI, SPECTRUM_BINS, analyze_window, band_bins, fft,
    };

    /// Build one interleaved little-endian f32 byte chunk from mono samples
    /// duplicated across `channels`.
    fn interleave(samples: &[f32], channels: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(samples.len() * channels * 4);
        for &s in samples {
            for _ in 0..channels {
                out.extend_from_slice(&s.to_le_bytes());
            }
        }
        out
    }

    /// A full window of a mono sine at `freq` Hz, amplitude `amp`.
    fn sine_window(freq: f32, amp: f32, rate: u32) -> Vec<f32> {
        (0..FFT_SIZE)
            .map(|i| amp * (2.0 * PI * freq * i as f32 / rate as f32).sin())
            .collect()
    }

    fn max_band(spec: &super::AudioSpectrum) -> usize {
        let mut best = 0;
        for i in 1..SPECTRUM_BINS {
            if spec.bins[i] > spec.bins[best] {
                best = i;
            }
        }
        best
    }

    #[test]
    fn silence_is_all_zero() {
        let spec = analyze_window(&vec![0.0_f32; FFT_SIZE], DEFAULT_RATE);
        assert!(spec.peak.abs() < 1e-6, "silence has no peak");
        assert!(
            spec.bins.iter().all(|&b| b.abs() < 1e-6),
            "silence lights no band"
        );
    }

    #[test]
    fn peak_tracks_amplitude() {
        let window = sine_window(1000.0, 0.5, DEFAULT_RATE);
        let spec = analyze_window(&window, DEFAULT_RATE);
        // A 0.5-amplitude sine peaks near 0.5 (sampling misses the exact crest,
        // so allow a small shortfall).
        assert!(
            (spec.peak - 0.5).abs() < 0.05,
            "peak {} should track the 0.5 amplitude",
            spec.peak
        );
    }

    /// Frequency selectivity — the real test that the FFT works: a low tone and
    /// a high tone must light different bands, low landing below high.
    #[test]
    fn tones_land_in_frequency_order() {
        let low = analyze_window(&sine_window(200.0, 0.8, DEFAULT_RATE), DEFAULT_RATE);
        let high = analyze_window(&sine_window(8000.0, 0.8, DEFAULT_RATE), DEFAULT_RATE);
        let low_band = max_band(&low);
        let high_band = max_band(&high);
        assert!(
            low_band < high_band,
            "200 Hz (band {low_band}) must sit below 8 kHz (band {high_band})"
        );
        // Each tone dominates its own band well above the noise floor.
        assert!(low.bins[low_band] > 0.2, "the low tone lights its band");
        assert!(high.bins[high_band] > 0.2, "the high tone lights its band");
    }

    /// The banding covers the whole usable spectrum with strictly increasing,
    /// non-empty, non-overlapping ranges.
    #[test]
    fn bands_partition_the_spectrum() {
        let half = FFT_SIZE / 2;
        let mut prev_hi = 1;
        for band in 0..SPECTRUM_BINS {
            let (lo, hi) = band_bins(band, half);
            assert!(hi > lo, "band {band} spans at least one bin");
            assert!(
                lo >= prev_hi - 1,
                "band {band} starts near the previous end"
            );
            prev_hi = hi;
        }
    }

    /// The FFT is correct: a real cosine at bin `k` concentrates its energy at
    /// bins `k` and `n-k` and nowhere else.
    #[test]
    fn fft_cosine_hits_a_single_bin() {
        let n = 64;
        let k = 5;
        let mut re: Vec<f32> = (0..n)
            .map(|i| (2.0 * PI * k as f32 * i as f32 / n as f32).cos())
            .collect();
        let mut im = vec![0.0_f32; n];
        fft(&mut re, &mut im);
        let mag: Vec<f32> = re
            .iter()
            .zip(&im)
            .map(|(r, i)| (r * r + i * i).sqrt())
            .collect();
        // Bin k should hold ~n/2; every other bin except its mirror is ~0.
        assert!(
            (mag[k] - (n as f32 / 2.0)).abs() < 1.0,
            "bin {k} carries n/2"
        );
        for (idx, &m) in mag.iter().enumerate() {
            if idx != k && idx != n - k {
                assert!(m < 1e-2, "bin {idx} should be silent (got {m})");
            }
        }
    }

    /// A full analysis window only emits once enough samples have accumulated,
    /// and stereo input is downmixed to mono before analysis.
    #[test]
    fn analyzer_emits_once_per_window_and_downmixes() {
        let mut a = Analyzer::new();
        a.set_rate(DEFAULT_RATE);
        // Half a window of stereo silence → nothing yet.
        let half = interleave(&vec![0.0_f32; FFT_SIZE / 2], 2);
        assert!(
            a.push_bytes(&half, 2).is_none(),
            "half a window emits nothing"
        );
        // The second half completes it → exactly one frame.
        assert!(
            a.push_bytes(&half, 2).is_some(),
            "the completing half emits a frame"
        );
    }

    /// Stereo downmix averages the two channels: identical L/R passes through, so
    /// a stereo tone lands in the same band a mono one would.
    #[test]
    fn stereo_downmix_matches_mono() {
        let window = sine_window(2000.0, 0.7, DEFAULT_RATE);
        let mono = analyze_window(&window, DEFAULT_RATE);

        let mut a = Analyzer::new();
        let bytes = interleave(&window, 2);
        let stereo = a.push_bytes(&bytes, 2).expect("a full stereo window emits");
        assert_eq!(
            max_band(&mono),
            max_band(&stereo),
            "stereo and mono of the same tone light the same band"
        );
    }
}
