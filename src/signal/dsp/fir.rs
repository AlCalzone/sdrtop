// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MusiThang <viktor.laszlo92@protonmail.com>

//! FIR design and streaming decimation.
//!
//! Two things, and they are separate for a reason: [`design_lowpass`] answers
//! "what kernel", [`StreamingDecimator`] answers "apply it across block
//! boundaries without a seam". A caller that needs a filter but not a decimator
//! should not have to build one.
//!
//! **The window here shapes a filter kernel, not a transform input.** That is
//! the whole difference between this module and [`super::window`], which does
//! the other job under the same word.

use num_complex::Complex;

/// Hamming-windowed sinc low-pass. `fc` is the cutoff in cycles/sample (< 0.5).
///
/// **The cutoff convention is the one thing to get right here**, and it is the
/// classic source of a silent factor of two: `fc` is in cycles per sample, so
/// `fc = 0.25` is a quarter of the sample rate and half of Nyquist. The design
/// places its -6 dB point there, which is what
/// `the_cutoff_sits_where_the_argument_says_it_does` pins.
pub fn design_lowpass(taps: usize, fc: f64) -> Vec<f32> {
    use std::f64::consts::PI;
    let taps = taps.max(1) | 1;
    let m = (taps - 1) as f64 / 2.0;
    let mut h = Vec::with_capacity(taps);
    let mut sum = 0.0f64;
    for i in 0..taps {
        let x = i as f64 - m;
        // sinc, with the removable singularity at the centre tap handled exactly.
        let sinc = if x.abs() < 1e-9 {
            2.0 * fc
        } else {
            (2.0 * PI * fc * x).sin() / (PI * x)
        };
        let w = 0.54 - 0.46 * (2.0 * PI * i as f64 / (taps - 1).max(1) as f64).cos();
        let v = sinc * w;
        sum += v;
        h.push(v);
    }
    // Normalise to unit DC gain so decimation does not change the level, and the
    // deviation figures stay in real Hz.
    if sum.abs() > 1e-12 {
        for v in h.iter_mut() {
            *v /= sum;
        }
    }
    h.into_iter().map(|v| v as f32).collect()
}

/// A decimating FIR that keeps its state between calls, so successive blocks
/// produce one seamless output stream.
///
/// A filter restarted at each block discards its first `taps` samples and resets
/// the decimation grid, which puts a small timing step at every block boundary.
/// Deviation statistics never notice; a narrowband tone detector does, because
/// its window spans several blocks and a phase step inside it destroys the
/// coherence the detection depends on. Anything that measures across a block
/// boundary needs this rather than a stateless filter.
pub struct StreamingDecimator {
    taps: Vec<f32>,
    d: usize,
    /// Input samples carried over so the next block's first output can see the
    /// full filter history.
    tail: Vec<Complex<f32>>,
    /// Where the decimation grid resumes inside the next block.
    phase: usize,
}

impl StreamingDecimator {
    pub fn new(taps: Vec<f32>, d: usize) -> Self {
        Self {
            taps,
            d: d.max(1),
            tail: Vec::new(),
            phase: 0,
        }
    }

    /// Forget the carried state - after a dropped block, or a parameter change.
    /// The next output block starts a fresh contiguous run.
    pub fn reset(&mut self) {
        self.tail.clear();
        self.phase = 0;
    }

    pub fn process(&mut self, input: &[Complex<f32>], out: &mut Vec<Complex<f32>>) {
        out.clear();
        let n = self.taps.len();
        if n == 0 || input.is_empty() {
            return;
        }

        // Splice the carried history in front of the new samples.
        let mut buf = std::mem::take(&mut self.tail);
        buf.extend_from_slice(input);
        if buf.len() < n {
            self.tail = buf;
            return;
        }

        let mut start = self.phase;
        while start + n <= buf.len() {
            let w = &buf[start..start + n];
            let mut acc = Complex {
                re: 0.0f32,
                im: 0.0f32,
            };
            for (s, &h) in w.iter().zip(self.taps.iter()) {
                acc.re += s.re * h;
                acc.im += s.im * h;
            }
            out.push(acc);
            start += self.d;
        }

        // Keep the samples the next output still needs, and remember where the
        // grid stands relative to them. When the stride overshoots the buffer
        // entirely, the leftover stride carries into the next block as phase.
        let consumed = start.min(buf.len());
        buf.drain(..consumed);
        self.phase = start - consumed;
        self.tail = buf;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::PI;

    /// Magnitude response at `f` cycles/sample. The kernel is real, so this is
    /// the plain DTFT sum and needs nothing from the FFT.
    fn response(h: &[f32], f: f64) -> f64 {
        let (mut re, mut im) = (0.0, 0.0);
        for (n, &c) in h.iter().enumerate() {
            let ph = -2.0 * PI * f * n as f64;
            re += c as f64 * ph.cos();
            im += c as f64 * ph.sin();
        }
        (re * re + im * im).sqrt()
    }

    fn db(x: f64) -> f64 {
        20.0 * x.log10()
    }

    /// A complex tone at `f` cycles/sample, unit amplitude.
    fn tone(f: f64, n: usize) -> Vec<Complex<f32>> {
        (0..n)
            .map(|i| {
                let ph = 2.0 * PI * f * i as f64;
                Complex {
                    re: ph.cos() as f32,
                    im: ph.sin() as f32,
                }
            })
            .collect()
    }

    /// The window method's own textbook figures, which is what makes this a
    /// design rather than a kernel that happened to work.
    ///
    /// A Hamming-windowed sinc has three published properties, none of which
    /// depends on the tap count: a transition width of `3.3 / N` normalised to
    /// the sample rate, at least 53 dB of stopband attenuation beyond it, and a
    /// passband ripple of about 0.02 dB. Asserting those rather than "the
    /// response is small somewhere out there" is what would catch a wrong window
    /// coefficient, a missing normalisation, or a sinc that is off by a factor
    /// of two.
    #[test]
    fn the_window_method_meets_its_own_textbook_figures() {
        for (taps, fc) in [(129usize, 0.05f64), (65, 0.1), (511, 0.0125)] {
            let h = design_lowpass(taps, fc);

            // Passband, over the flat half of it. The shoulder near the cutoff
            // is the transition and is measured by the width rule below, not
            // here.
            let (mut lo, mut hi) = (f64::INFINITY, 0.0f64);
            for k in 0..=200 {
                let m = response(&h, 0.5 * fc * k as f64 / 200.0);
                lo = lo.min(m);
                hi = hi.max(m);
            }
            assert!(
                db(hi / lo) < 0.0194,
                "taps={taps} fc={fc}: passband ripple {:.4} dB exceeds Hamming's 0.0194",
                db(hi / lo)
            );

            // Stopband, from one transition width above the cutoff to Nyquist.
            let edge = fc + 3.3 / taps as f64;
            let mut worst = 0.0f64;
            let mut f = edge;
            while f <= 0.5 {
                worst = worst.max(response(&h, f));
                f += 1e-4;
            }
            assert!(
                db(worst) <= -53.0,
                "taps={taps} fc={fc}: stopband {:.2} dB is worse than Hamming's -53",
                db(worst)
            );
        }
    }

    /// The cutoff convention, which is the one thing in a filter API that goes
    /// wrong silently.
    ///
    /// `fc` is cycles per sample, so a windowed sinc puts its **-6 dB** point
    /// exactly there. A design that took `fc` as a fraction of Nyquist instead
    /// would put it at `fc / 2` and every filter in the app would be twice as
    /// narrow as its caller believed, with nothing to show for it but a slightly
    /// quiet signal.
    #[test]
    fn the_cutoff_sits_where_the_argument_says_it_does() {
        for (taps, fc) in [(129usize, 0.05f64), (65, 0.1), (511, 0.0125)] {
            let h = design_lowpass(taps, fc);
            let at_fc = response(&h, fc);
            assert!(
                (at_fc - 0.5).abs() < 0.01,
                "taps={taps} fc={fc}: |H(fc)| = {at_fc:.4}, the -6 dB point is elsewhere"
            );
        }
    }

    /// Unit DC gain, so decimating never changes a level and the figures
    /// downstream stay in real units.
    #[test]
    fn a_lowpass_has_unit_dc_gain() {
        for (taps, fc) in [(65usize, 0.05f64), (31, 0.2), (511, 0.01)] {
            let dc: f32 = design_lowpass(taps, fc).iter().sum();
            assert!((dc - 1.0).abs() < 1e-4, "taps={taps}: DC gain = {dc}");
        }
    }

    /// An impulse in gives the kernel back, which is the definition of the
    /// thing and pins two mistakes at once.
    ///
    /// The inner loop pairs `taps[k]` with `window[k]` rather than with
    /// `window[n-1-k]`, so it is a correlation and not a convolution. For a
    /// symmetric kernel those are the same, and this asserts both halves of
    /// that: the response is the kernel, **and** the kernel is symmetric. Break
    /// the symmetry and the two stop agreeing, which is exactly when a
    /// correlation dressed as a convolution starts mattering.
    #[test]
    fn an_impulse_comes_back_out_as_the_kernel() {
        let h = design_lowpass(65, 0.1);
        let n = h.len();
        for (i, &t) in h.iter().enumerate() {
            assert!(
                (t - h[n - 1 - i]).abs() < 1e-7,
                "tap {i} breaks symmetry: {t} vs {}",
                h[n - 1 - i]
            );
        }

        let mut input = vec![
            Complex {
                re: 0.0f32,
                im: 0.0
            };
            2 * n
        ];
        input[n - 1] = Complex { re: 1.0, im: 0.0 };
        let mut out = Vec::new();
        StreamingDecimator::new(h.clone(), 1).process(&input, &mut out);

        assert!(out.len() > n, "not enough output to see the whole kernel");
        for (i, tap) in h.iter().enumerate() {
            assert!(
                (out[i].re - h[n - 1 - i]).abs() < 1e-6 && out[i].im.abs() < 1e-6,
                "sample {i} is {} and should be {tap}",
                out[i]
            );
        }
    }

    /// A constant in is the same constant out, at any decimation. This is unit
    /// DC gain observed through the decimator rather than asserted on the taps,
    /// which is where a caller would actually notice it going wrong.
    #[test]
    fn a_constant_survives_decimation_unchanged() {
        let h = design_lowpass(63, 0.05);
        for d in [1usize, 2, 8, 40] {
            let input = vec![
                Complex {
                    re: 0.25f32,
                    im: -0.75
                };
                4096
            ];
            let mut out = Vec::new();
            StreamingDecimator::new(h.clone(), d).process(&input, &mut out);
            assert!(!out.is_empty(), "d={d} produced nothing");
            for (i, v) in out.iter().enumerate() {
                assert!(
                    (v.re - 0.25).abs() < 1e-4 && (v.im + 0.75).abs() < 1e-4,
                    "d={d} sample {i} is {v}"
                );
            }
        }
    }

    /// A block shorter than the filter cannot produce an output, and must carry
    /// forward rather than emit a half-warmed sample.
    #[test]
    fn decimating_is_a_noop_when_the_input_is_shorter_than_the_filter() {
        let mut sd = StreamingDecimator::new(design_lowpass(63, 0.1), 4);
        let mut out = Vec::new();
        sd.process(&tone(0.01, 10), &mut out);
        assert!(out.is_empty());
    }

    /// **The property every measurement spanning a block boundary rests on.**
    /// The same samples, delivered whole or in ragged pieces, must give the same
    /// output: same values, same count, no timing step at the seams. None of the
    /// piece sizes is a multiple of the decimation factor, which is the case
    /// that exercises the carried phase.
    #[test]
    fn feeding_in_ragged_pieces_matches_one_long_block() {
        let d = 8;
        let taps = design_lowpass(133, 0.4 / d as f64);
        let iq = tone(0.0025, 1 << 15);

        let mut whole = Vec::new();
        StreamingDecimator::new(taps.clone(), d).process(&iq, &mut whole);

        for pieces in [3_001usize, 101, 7, 999, 4_097, 1, 65_536] {
            let mut sd = StreamingDecimator::new(taps.clone(), d);
            let (mut pieced, mut part) = (Vec::new(), Vec::new());
            for chunk in iq.chunks(pieces) {
                sd.process(chunk, &mut part);
                pieced.extend_from_slice(&part);
            }
            assert_eq!(
                pieced.len(),
                whole.len(),
                "chunk size {pieces}: sample count diverged"
            );
            for (i, (a, b)) in pieced.iter().zip(whole.iter()).enumerate() {
                assert!(
                    (a - b).norm() < 1e-4,
                    "chunk size {pieces}, sample {i}: {a} vs {b}"
                );
            }
        }
    }

    /// After a reset the filter has no history, so it re-warms exactly as it did
    /// the first time rather than splicing onto stale samples.
    #[test]
    fn a_reset_starts_a_fresh_run() {
        let mut sd = StreamingDecimator::new(design_lowpass(31, 0.1), 4);
        let iq = tone(0.005, 4096);
        let (mut a, mut b) = (Vec::new(), Vec::new());
        sd.process(&iq, &mut a);
        sd.reset();
        sd.process(&iq, &mut b);
        assert_eq!(a.len(), b.len());
        for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
            assert!((x - y).norm() < 1e-6, "sample {i}: {x} vs {y}");
        }
    }
}
