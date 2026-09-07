// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MusiThang <viktor.laszlo92@protonmail.com>

use std::sync::{Arc, Mutex};
use std::time::Instant;

use crossbeam_channel::Receiver;

use crate::hardware::PowerTrace;
use crate::state::{FftFrame, SdrMetrics};

const EMA_ALPHA: f32 = 0.2;
const PEAK_DECAY_DB: f32 = 0.5;

pub struct PowerWorker {
    trace_rx: Receiver<PowerTrace>,
    state: Arc<Mutex<SdrMetrics>>,
}

impl PowerWorker {
    pub fn new(trace_rx: Receiver<PowerTrace>, state: Arc<Mutex<SdrMetrics>>) -> Self {
        Self { trace_rx, state }
    }

    pub fn run(self) {
        let mut accumulator = SpectrumAccumulator::default();
        while let Ok(trace) = self.trace_rx.recv() {
            if trace.frequencies_hz.is_empty()
                || trace.frequencies_hz.len() != trace.levels_dbm.len()
            {
                continue;
            }
            accumulator.publish(&self.state, trace);
        }
    }
}

#[derive(Default)]
struct SpectrumAccumulator {
    start_hz: u64,
    stop_hz: u64,
    smoothed: Vec<f32>,
    peak: Vec<f32>,
}

impl SpectrumAccumulator {
    fn publish(&mut self, state: &Arc<Mutex<SdrMetrics>>, trace: PowerTrace) {
        let start_hz = trace.frequencies_hz[0];
        let stop_hz = *trace.frequencies_hz.last().unwrap_or(&start_hz);
        let reset = self.start_hz != start_hz
            || self.stop_hz != stop_hz
            || self.smoothed.len() != trace.levels_dbm.len();
        if reset {
            self.start_hz = start_hz;
            self.stop_hz = stop_hz;
            self.smoothed.clone_from(&trace.levels_dbm);
            self.peak.clone_from(&trace.levels_dbm);
        } else {
            for ((smoothed, peak), level) in self
                .smoothed
                .iter_mut()
                .zip(self.peak.iter_mut())
                .zip(trace.levels_dbm.iter().copied())
            {
                *smoothed += EMA_ALPHA * (level - *smoothed);
                *peak = (*peak - PEAK_DECAY_DB).max(level);
            }
        }

        let noise_floor = noise_floor(&self.smoothed);
        let strongest = self
            .smoothed
            .iter()
            .copied()
            .filter(|value| value.is_finite())
            .fold(f32::NEG_INFINITY, f32::max);
        let prominence = if strongest.is_finite() && noise_floor.is_finite() {
            strongest - noise_floor
        } else {
            0.0
        };

        let mut metrics = state.lock().unwrap_or_else(|error| error.into_inner());
        let (center_freq_hz, sample_rate) = trace_window(&trace.frequencies_hz)
            .unwrap_or((metrics.radio.frequency, metrics.radio.config_sample_rate));
        metrics.radio.frequency = center_freq_hz;
        let bins = Arc::new(self.smoothed.clone());
        let peak = Arc::new(self.peak.clone());
        metrics.waterfall.buffer.push(&bins);
        metrics.signal.peak_to_nf_db = prominence;
        metrics.signal.channel_power_dbfs = strongest;
        metrics.waterfall.last_fft = Some(FftFrame {
            bins_dbfs: bins,
            peak_hold: peak,
            noise_floor,
            center_freq_hz,
            sample_rate,
            timestamp: Instant::now(),
            peak_to_nf_db: prominence,
            channel_power_dbfs: strongest,
            occupied_bw_hz: 0,
            enbw_hz: trace.rbw_hz.unwrap_or(0) as f64,
        });
    }
}

fn noise_floor(levels: &[f32]) -> f32 {
    let mut finite: Vec<f32> = levels
        .iter()
        .copied()
        .filter(|value| value.is_finite())
        .collect();
    if finite.is_empty() {
        return f32::NEG_INFINITY;
    }
    finite.sort_by(f32::total_cmp);
    finite[finite.len() / 5]
}

fn trace_window(frequencies_hz: &[u64]) -> Option<(u64, f64)> {
    let start = *frequencies_hz.first()?;
    let last = *frequencies_hz.last()?;
    let step = frequencies_hz
        .get(frequencies_hz.len().checked_sub(2)?)
        .map(|previous| last.saturating_sub(*previous))
        .filter(|step| *step > 0)?;
    let stop = last.saturating_add(step);
    let span = stop.saturating_sub(start);
    Some((start.saturating_add(span / 2), span as f64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noise_floor_uses_the_quiet_part_of_the_trace() {
        let mut levels = vec![-100.0; 80];
        levels.extend([-30.0; 20]);
        assert_eq!(noise_floor(&levels), -100.0);
    }

    #[test]
    fn trace_window_uses_the_measured_edges() {
        assert_eq!(
            trace_window(&[100_000, 200_000, 300_000]),
            Some((250_000, 300_000.0))
        );
        assert_eq!(trace_window(&[100_000]), None);
    }
}
