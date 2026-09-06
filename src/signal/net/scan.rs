// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MusiThang <viktor.laszlo92@protonmail.com>

//! Blocks of bytes into a plane of per-cell powers, and that plane into a duty
//! cycle for every megahertz of the band that was in view.
//!
//! The measurement itself is [`super::occupancy`], which is pure and knows
//! nothing about transforms or sample formats. What lives here is the part that
//! cannot be: the transform, the tuning it was taken at, and the accumulation
//! across a dwell.
//!
//! **The window is a fixed length of time, not a fixed number of samples.** A
//! duty cycle measured in six-microsecond windows on one radio and in
//! forty-microsecond windows on another is two different quantities wearing one
//! label, and rule 5 is exactly about that. [`WINDOW_S`] is the length; the
//! transform size follows from it and the sample rate.

use std::sync::Arc;

use rustfft::{num_complex::Complex, Fft, FftPlanner};

use crate::hardware::SampleGeometry;
use crate::signal::dsp::{compute_window, WindowFn};
use crate::signal::fft::frame::decode_into;
use crate::state::CellReading;

use super::occupancy::{self, Floor};

/// How long one measurement window is.
///
/// Eight microseconds. Short enough to resolve the shortest thing in the band
/// worth calling a burst - an 802.11 OFDM symbol is four microseconds and a
/// short preamble is eight - and long enough that a cell holds several transform
/// bins at every sample rate this section admits.
pub const WINDOW_S: f64 = 8e-6;

/// Smallest transform the scan will use.
///
/// At the bottom of the admitted sample rates, eight microseconds is fewer
/// samples than a transform can usefully resolve the band into. Sixteen bins is
/// the floor; below it a cell would be a fraction of a bin and the mapping would
/// be a fiction.
const MIN_BINS: usize = 16;

/// What one tuning's worth of scanning needs, built once and reused.
///
/// Rebuilt when the tuning, the rate or the usable span changes, because every
/// one of those changes which cell a bin belongs to. Nothing here is rebuilt per
/// block.
pub struct Scan {
    fft: Arc<dyn Fft<f32>>,
    /// Transform size, and the window applied before it.
    n: usize,
    window: Vec<f32>,
    /// `(sum of the window)^2`, the gain a full-scale tone would pick up. Cell
    /// powers are divided by it, so a cell holding a full-scale carrier reads
    /// 0 dBFS whatever the transform size.
    window_gain: f64,
    /// Which cell each bin lands in, or `None` for one outside the usable span.
    bin_cells: Vec<Option<usize>>,
    /// The tuning this was built for.
    centre_hz: f64,
    rate_hz: f64,
    span_hz: f64,
    /// Scratch, so a steady state allocates nothing.
    samples: Vec<Complex<f32>>,
    /// The per-window, per-cell power plane for one block.
    plane: Vec<f64>,
    /// Per-cell totals across the dwell.
    windows: Vec<u64>,
    busy: Vec<u64>,
    power_sum: Vec<f64>,
    peak: Vec<f64>,
    /// Running mean of the floors the blocks in this dwell were measured
    /// against, and the shape statistics that decided whether to believe them.
    floor_sum: f64,
    tail_sum: f64,
    spread_sum: f64,
    floors: u64,
    trusted: bool,
}

/// The transform size for a rate: the power of two closest to [`WINDOW_S`].
///
/// A power of two rather than the exact sample count because the transform is
/// run tens of thousands of times a second and a mixed-radix size of 157 costs
/// more than the eight percent of window length it would buy back.
pub fn bins_for(rate_hz: f64) -> usize {
    if !rate_hz.is_finite() || rate_hz <= 0.0 {
        return MIN_BINS;
    }
    let want = rate_hz * WINDOW_S;
    let n = 1usize << (want.max(1.0).log2().round() as u32);
    n.max(MIN_BINS)
}

impl Scan {
    pub fn new(centre_hz: f64, rate_hz: f64, span_hz: f64) -> Self {
        let n = bins_for(rate_hz);
        let window = compute_window(WindowFn::Hann, n);
        let sum: f64 = window.iter().map(|w| *w as f64).sum();
        Self {
            fft: FftPlanner::<f32>::new().plan_fft_forward(n),
            n,
            window,
            window_gain: sum * sum,
            bin_cells: occupancy::bin_cells(centre_hz, rate_hz, span_hz, n),
            centre_hz,
            rate_hz,
            span_hz,
            samples: vec![Complex::default(); n],
            plane: Vec::new(),
            windows: vec![0; occupancy::CELLS],
            busy: vec![0; occupancy::CELLS],
            power_sum: vec![0.0; occupancy::CELLS],
            peak: vec![0.0; occupancy::CELLS],
            floor_sum: 0.0,
            tail_sum: 0.0,
            spread_sum: 0.0,
            floors: 0,
            trusted: true,
        }
    }

    /// How long this dwell has actually been looking at the band.
    ///
    /// Windows, not wall clock: a dwell interrupted by dropped blocks is a
    /// shorter dwell, not a diluted one.
    pub fn observed_s(&self) -> f64 {
        let widest = self.windows.iter().copied().max().unwrap_or(0);
        widest as f64 * self.n as f64 / self.rate_hz.max(1.0)
    }

    /// Whether this scan was built for the tuning now in force.
    pub fn matches(&self, centre_hz: f64, rate_hz: f64, span_hz: f64) -> bool {
        self.centre_hz == centre_hz && self.rate_hz == rate_hz && self.span_hz == span_hz
    }

    /// Fold one block of interleaved bytes into the dwell.
    ///
    /// The floor is derived per block rather than per dwell, and the counts are
    /// what accumulate. A dwell's worth of raw powers would be megabytes to hold
    /// and to select over; a block's is ten thousand samples, which is enough
    /// for a floor good to a few tenths of a decibel, and deriving it afresh
    /// means a gain change part way through a dwell does not poison the rest of
    /// it.
    pub fn push(&mut self, bytes: &[u8], geometry: SampleGeometry) {
        let pair_bytes = geometry.bytes_per_pair();
        let stride = self.n * pair_bytes;
        let windows = bytes.len() / stride;
        if windows == 0 || self.bin_cells.iter().all(Option::is_none) {
            return;
        }

        // The plane is window-major: one row per window, one column per cell
        // that any bin lands in. Cells nothing lands in are not columns.
        let observed = occupancy::cells_observed(self.centre_hz, self.span_hz);
        let width = observed.len();
        self.plane.clear();
        self.plane.resize(windows * width, 0.0);

        for w in 0..windows {
            let frame = &bytes[w * stride..(w + 1) * stride];
            decode_into(frame, &self.window, geometry, &mut self.samples);
            self.fft.process(&mut self.samples);
            let row = &mut self.plane[w * width..(w + 1) * width];
            for (bin, cell) in self.bin_cells.iter().enumerate() {
                if let Some(cell) = cell {
                    let x = self.samples[bin];
                    row[cell - observed.start] +=
                        (x.re as f64 * x.re as f64 + x.im as f64 * x.im as f64) / self.window_gain;
                }
            }
        }

        let mut scratch = self.plane.clone();
        let Some(floor) = occupancy::derive_floor(&mut scratch) else {
            return;
        };
        self.record(&floor, &observed, windows, width);
    }

    fn record(
        &mut self,
        floor: &Floor,
        observed: &std::ops::Range<usize>,
        windows: usize,
        width: usize,
    ) {
        self.floor_sum += floor.power;
        self.tail_sum += floor.tail;
        self.spread_sum += floor.spread;
        self.floors += 1;
        // One untrustworthy block makes the dwell untrustworthy. The alternative
        // is averaging a verdict, and half a saturated dwell is not half a
        // measurement.
        self.trusted &= floor.trusted;

        for w in 0..windows {
            let row = &self.plane[w * width..(w + 1) * width];
            for (i, power) in row.iter().enumerate() {
                let cell = observed.start + i;
                self.windows[cell] += 1;
                self.busy[cell] += u64::from(*power > floor.threshold);
                self.power_sum[cell] += *power;
                self.peak[cell] = self.peak[cell].max(*power);
            }
        }
    }

    /// The dwell so far, as the state carries it, and start again.
    pub fn take(&mut self) -> crate::state::BandOccupancy {
        let cells = (0..occupancy::CELLS)
            .map(|c| CellReading {
                windows: self.windows[c],
                duty: occupancy::duty_cycle(self.busy[c], self.windows[c]),
                mean_dbfs: dbfs(if self.windows[c] > 0 {
                    self.power_sum[c] / self.windows[c] as f64
                } else {
                    0.0
                }),
                peak_dbfs: dbfs(self.peak[c]),
                // A dwell knows nothing about how often it happens. Coverage
                // and the time of measurement are `BandOccupancy::absorb`'s to
                // fill in, because both are about the sequence of dwells rather
                // than about this one.
                coverage: None,
                measured: None,
            })
            .collect();
        let n = self.floors.max(1) as f64;
        let out = crate::state::BandOccupancy {
            cells,
            noise_dbfs: (self.floors > 0).then(|| dbfs(self.floor_sum / n)),
            trusted: self.floors > 0 && self.trusted,
            tail: self.tail_sum / n,
            spread: self.spread_sum / n,
            window_s: self.n as f64 / self.rate_hz.max(1.0),
        };
        self.reset();
        out
    }

    fn reset(&mut self) {
        self.windows.fill(0);
        self.busy.fill(0);
        self.power_sum.fill(0.0);
        self.peak.fill(0.0);
        self.floor_sum = 0.0;
        self.tail_sum = 0.0;
        self.spread_sum = 0.0;
        self.floors = 0;
        self.trusted = true;
    }
}

/// Linear power to dBFS, with a floor rather than a negative infinity.
///
/// The floor is the same one the spectrum uses, so a cell nothing was heard in
/// reads the same "nothing" everywhere in the app. Rule 5.
fn dbfs(power: f64) -> f64 {
    if power > 0.0 {
        (10.0 * power.log10()).max(crate::signal::fft::DB_FLOOR as f64)
    } else {
        crate::signal::fft::DB_FLOOR as f64
    }
}
