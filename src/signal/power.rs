// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MusiThang <viktor.laszlo92@protonmail.com>

use std::sync::{Arc, Mutex};
use std::time::Instant;

use crossbeam_channel::Receiver;

use crate::hardware::{PowerTrace, PowerTraceTarget};
use crate::state::{FftFrame, SdrMetrics, SweepFrame};

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
        let mut spectrum = SpectrumAccumulator::default();
        let mut sweep = SweepAccumulator::default();

        while let Ok(trace) = self.trace_rx.recv() {
            if trace.frequencies_hz.is_empty()
                || trace.frequencies_hz.len() != trace.levels_dbm.len()
            {
                continue;
            }
            match trace.target {
                PowerTraceTarget::Spectrum => spectrum.publish(&self.state, trace),
                PowerTraceTarget::Sweep => sweep.push(&self.state, trace),
            }
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
            .filter(|v| v.is_finite())
            .fold(f32::NEG_INFINITY, f32::max);
        let prominence = if strongest.is_finite() && noise_floor.is_finite() {
            strongest - noise_floor
        } else {
            0.0
        };

        let mut m = state.lock().unwrap_or_else(|e| e.into_inner());
        let (center_freq_hz, sample_rate) = trace_window(&trace.frequencies_hz)
            .unwrap_or((m.radio.frequency, m.radio.config_sample_rate));
        let bins = Arc::new(self.smoothed.clone());
        let peak = Arc::new(self.peak.clone());
        m.waterfall.buffer.push(&bins);
        m.signal.peak_to_nf_db = prominence;
        m.signal.channel_power_dbfs = strongest;
        m.waterfall.last_fft = Some(FftFrame {
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

#[derive(Default)]
struct SweepAccumulator {
    generation: u64,
    start_hz: u64,
    stop_hz: u64,
    frequencies_hz: Vec<u64>,
    sum: Vec<f64>,
    peak: Vec<f32>,
    count: u32,
    started: Option<Instant>,
}

impl SweepAccumulator {
    fn push(&mut self, state: &Arc<Mutex<SdrMetrics>>, trace: PowerTrace) {
        let requested = {
            let m = state.lock().unwrap_or_else(|e| e.into_inner());
            (
                m.sweep.config.start_hz,
                m.sweep.config.stop_hz,
                m.sweep.config.dwell_ms,
                m.sweep.active,
                m.sweep.generation,
            )
        };
        if !requested.3 {
            self.reset();
            return;
        }
        if trace.generation != requested.4 {
            return;
        }

        let changed = self.start_hz != requested.0
            || self.stop_hz != requested.1
            || self.frequencies_hz != trace.frequencies_hz
            || self.generation != trace.generation;
        if changed {
            self.generation = trace.generation;
            self.start_hz = requested.0;
            self.stop_hz = requested.1;
            self.frequencies_hz.clone_from(&trace.frequencies_hz);
            self.sum = vec![0.0; trace.levels_dbm.len()];
            self.peak = vec![f32::NEG_INFINITY; trace.levels_dbm.len()];
            self.count = 0;
            self.started = Some(Instant::now());
        }

        for ((sum, peak), level) in self
            .sum
            .iter_mut()
            .zip(self.peak.iter_mut())
            .zip(trace.levels_dbm.iter().copied())
        {
            *sum += level as f64;
            *peak = peak.max(level);
        }
        self.count += 1;

        let elapsed = self.started.map(|t| t.elapsed()).unwrap_or_default();
        {
            let mut m = state.lock().unwrap_or_else(|e| e.into_inner());
            m.sweep.positions_total = self.frequencies_hz.len();
            m.sweep.positions_done = self.frequencies_hz.len();
            m.sweep.current_hz = *self.frequencies_hz.last().unwrap_or(&self.start_hz);
        }
        if elapsed.as_millis() < requested.2 as u128 {
            return;
        }

        let count = self.count.max(1) as f64;
        let mean = self.sum.iter().map(|sum| (*sum / count) as f32).collect();
        let duration_ms = elapsed.as_millis() as u64;
        let mut m = state.lock().unwrap_or_else(|e| e.into_inner());
        if !m.sweep.active {
            self.reset();
            return;
        }
        m.sweep.cycle_count += 1;
        m.sweep.cycle_duration_ms = duration_ms;
        m.sweep.current_frame = Some(Arc::new(SweepFrame {
            start_hz: self.start_hz,
            stop_hz: self.stop_hz,
            freq_hz: self.frequencies_hz.clone(),
            peak_dbfs: self.peak.clone(),
            mean_dbfs: mean,
            timestamp: Instant::now(),
            cycle_count: m.sweep.cycle_count,
            cycle_duration_ms: duration_ms,
        }));
        drop(m);

        self.sum.fill(0.0);
        self.peak.fill(f32::NEG_INFINITY);
        self.count = 0;
        self.started = Some(Instant::now());
    }

    fn reset(&mut self) {
        *self = Self::default();
    }
}

fn noise_floor(levels: &[f32]) -> f32 {
    let mut finite: Vec<f32> = levels.iter().copied().filter(|v| v.is_finite()).collect();
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
    fn noise_floor_declines_an_empty_trace() {
        assert_eq!(noise_floor(&[]), f32::NEG_INFINITY);
    }

    #[test]
    fn trace_window_uses_the_measured_edges() {
        assert_eq!(
            trace_window(&[100_000, 200_000, 300_000]),
            Some((250_000, 300_000.0))
        );
        assert_eq!(trace_window(&[100_000]), None);
    }

    #[test]
    fn a_new_sweep_generation_discards_partial_accumulation() {
        let state = Arc::new(Mutex::new(crate::state::SdrMetrics::fixture()));
        {
            let mut m = state.lock().unwrap();
            m.sweep.active = true;
            m.sweep.config.start_hz = 100_000_000;
            m.sweep.config.stop_hz = 101_000_000;
            m.sweep.config.dwell_ms = 10_000;
            m.sweep.generation = 1;
        }
        let trace = |generation| PowerTrace {
            target: PowerTraceTarget::Sweep,
            generation,
            frequencies_hz: vec![100_000_000, 100_500_000],
            levels_dbm: vec![-80.0, -70.0],
            rbw_hz: None,
        };
        let mut accumulator = SweepAccumulator::default();
        accumulator.push(&state, trace(1));
        assert_eq!(accumulator.count, 1);

        state.lock().unwrap().sweep.generation = 2;
        accumulator.push(&state, trace(1));
        assert_eq!(accumulator.generation, 1);
        assert_eq!(accumulator.count, 1);

        accumulator.push(&state, trace(2));
        assert_eq!(accumulator.generation, 2);
        assert_eq!(accumulator.count, 1);
        assert_eq!(accumulator.sum, vec![-80.0, -70.0]);
    }
}
