// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MusiThang <viktor.laszlo92@protonmail.com>

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;

use crate::hardware::PowerTrace;
use crate::state::{FftFrame, SdrMetrics};

const EMA_ALPHA: f32 = 0.2;
const PEAK_DECAY_DB: f32 = 0.5;
const REJECTION_REPORT_INTERVAL: Duration = Duration::from_secs(5);

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
        let mut rejection_reporter = RejectionReporter::default();
        while let Ok(trace) = self.trace_rx.recv() {
            if let Err(reason) = accumulator.publish(&self.state, trace) {
                rejection_reporter.report(&self.state, reason, Instant::now());
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TraceRejection {
    Empty,
    LengthMismatch,
    NonUniformGrid,
}

impl TraceRejection {
    fn message(self) -> &'static str {
        match self {
            Self::Empty => "empty trace",
            Self::LengthMismatch => "frequency and level counts differ",
            Self::NonUniformGrid => "frequency grid is not uniform and ascending",
        }
    }
}

#[derive(Default)]
struct RejectionReporter {
    last_report: Option<Instant>,
    suppressed: usize,
}

impl RejectionReporter {
    fn report(&mut self, state: &Arc<Mutex<SdrMetrics>>, reason: TraceRejection, now: Instant) {
        let should_report = self
            .last_report
            .is_none_or(|last| now.duration_since(last) >= REJECTION_REPORT_INTERVAL);
        if !should_report {
            self.suppressed += 1;
            return;
        }

        let suffix = match self.suppressed {
            0 => String::new(),
            1 => "; 1 additional trace rejected".to_string(),
            count => format!("; {count} additional traces rejected"),
        };
        let mut metrics = state.lock().unwrap_or_else(|error| error.into_inner());
        metrics.push_log(format!(
            "Power trace rejected: {}{suffix}",
            reason.message()
        ));
        self.last_report = Some(now);
        self.suppressed = 0;
    }
}

#[derive(Default)]
struct SpectrumAccumulator {
    start_hz: u64,
    stop_hz: u64,
    rbw_hz: Option<u32>,
    smoothed: Vec<f32>,
    peak: Vec<f32>,
    noise_scratch: Vec<f32>,
}

impl SpectrumAccumulator {
    fn publish(
        &mut self,
        state: &Arc<Mutex<SdrMetrics>>,
        trace: PowerTrace,
    ) -> Result<(), TraceRejection> {
        if trace.frequencies_hz.is_empty() {
            return Err(TraceRejection::Empty);
        }
        if trace.frequencies_hz.len() != trace.levels_dbm.len() {
            return Err(TraceRejection::LengthMismatch);
        }
        let Some((center_freq_hz, sample_rate)) = trace_window(&trace.frequencies_hz) else {
            return Err(TraceRejection::NonUniformGrid);
        };
        let start_hz = trace.frequencies_hz[0];
        let stop_hz = *trace.frequencies_hz.last().unwrap_or(&start_hz);
        let reset = self.start_hz != start_hz
            || self.stop_hz != stop_hz
            || self.rbw_hz != trace.rbw_hz
            || self.smoothed.len() != trace.levels_dbm.len();
        if reset {
            self.start_hz = start_hz;
            self.stop_hz = stop_hz;
            self.rbw_hz = trace.rbw_hz;
            self.smoothed
                .resize(trace.levels_dbm.len(), f32::NEG_INFINITY);
            self.peak.resize(trace.levels_dbm.len(), f32::NEG_INFINITY);
        }
        crate::signal::stats::average_and_peak(
            &trace.levels_dbm,
            &mut self.smoothed,
            &mut self.peak,
            EMA_ALPHA,
            PEAK_DECAY_DB,
            !reset,
        );

        self.noise_scratch.resize(self.smoothed.len(), 0.0);
        let noise_floor =
            crate::signal::stats::quietest_mean(&self.smoothed, &mut self.noise_scratch, 10);
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

        let bins = Arc::new(self.smoothed.clone());
        let peak = Arc::new(self.peak.clone());
        let mut metrics = state.lock().unwrap_or_else(|error| error.into_inner());
        metrics.waterfall.buffer.push(&bins);
        metrics.signal.peak_to_nf_db = prominence;
        metrics.waterfall.last_fft = Some(FftFrame {
            bins_dbfs: bins,
            peak_hold: peak,
            noise_floor,
            center_freq_hz,
            sample_rate,
            timestamp: Instant::now(),
            peak_to_nf_db: prominence,
            channel_power_dbfs: f32::NEG_INFINITY,
            occupied_bw_hz: 0,
            enbw_hz: trace.rbw_hz.unwrap_or(0) as f64,
            bin_axis: crate::state::BinAxis::MeasuredPoints,
        });
        Ok(())
    }
}

pub(crate) fn trace_window(frequencies_hz: &[u64]) -> Option<(u64, f64)> {
    let start = *frequencies_hz.first()?;
    let stop = *frequencies_hz.last()?;
    let intervals = frequencies_hz.len().checked_sub(1)?;
    let span = stop.checked_sub(start)?;
    if span == 0 {
        return None;
    }
    let low_step = span / intervals as u64;
    let high_step = span.div_ceil(intervals as u64);
    if !frequencies_hz.windows(2).all(|pair| {
        pair[1]
            .checked_sub(pair[0])
            .is_some_and(|step| step > 0 && (low_step..=high_step).contains(&step))
    }) {
        return None;
    }
    Some((start.saturating_add(span / 2), span as f64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_worker_publishes_spectrum_and_waterfall_frames() {
        let state = Arc::new(Mutex::new(SdrMetrics::fixture()));
        let (tx, rx) = crossbeam_channel::bounded(1);
        let worker_state = Arc::clone(&state);
        let worker = std::thread::spawn(move || PowerWorker::new(rx, worker_state).run());

        tx.send(PowerTrace {
            frequencies_hz: vec![100_000_000, 101_000_000, 102_000_000],
            levels_dbm: vec![-90.0, -45.0, -80.0],
            rbw_hz: Some(10_000),
        })
        .unwrap();
        drop(tx);
        worker.join().unwrap();

        let metrics = state.lock().unwrap_or_else(|error| error.into_inner());
        let frame = metrics.waterfall.last_fft.as_ref().unwrap();
        assert_eq!(frame.bins_dbfs.as_slice(), &[-90.0, -45.0, -80.0]);
        assert_eq!(frame.peak_hold.as_slice(), &[-90.0, -45.0, -80.0]);
        assert_eq!(frame.center_freq_hz, 101_000_000);
        assert_eq!(frame.sample_rate, 2_000_000.0);
        assert_eq!(frame.enbw_hz, 10_000.0);
        assert_eq!(metrics.waterfall.buffer.rows.len(), 1);
        assert_eq!(metrics.signal.peak_to_nf_db, 45.0);
        assert_eq!(metrics.radio.frequency, 100_000_000);
        assert_eq!(metrics.signal.channel_power_dbfs, f32::NEG_INFINITY);
        assert_eq!(frame.channel_power_dbfs, f32::NEG_INFINITY);
        assert!(metrics
            .ui
            .log
            .iter()
            .all(|entry| !entry.text.contains("Power trace rejected")));
    }

    #[test]
    fn noise_floor_uses_the_quiet_part_of_the_trace() {
        let mut levels = vec![-100.0; 80];
        levels.extend([-30.0; 20]);
        let mut scratch = vec![0.0; levels.len()];
        assert_eq!(
            crate::signal::stats::quietest_mean(&levels, &mut scratch, 10),
            -100.0
        );
    }

    #[test]
    fn smoothing_and_peak_hold_stay_ordered() {
        let state = Arc::new(Mutex::new(SdrMetrics::fixture()));
        let mut accumulator = SpectrumAccumulator::default();
        for levels in [[-90.0, -80.0], [-91.0, -70.0], [f32::NAN, -72.0]] {
            accumulator
                .publish(
                    &state,
                    PowerTrace {
                        frequencies_hz: vec![100_000_000, 101_000_000],
                        levels_dbm: levels.to_vec(),
                        rbw_hz: None,
                    },
                )
                .unwrap();
        }

        let metrics = state.lock().unwrap_or_else(|error| error.into_inner());
        let frame = metrics.waterfall.last_fft.as_ref().unwrap();
        assert!((frame.bins_dbfs[0] - -90.2).abs() < 1e-4);
        assert!((frame.bins_dbfs[1] - -76.8).abs() < 1e-4);
        assert!((frame.peak_hold[0] - -90.2).abs() < 1e-4);
        assert!((frame.peak_hold[1] - -76.8).abs() < 1e-4);
        for (peak, smoothed) in frame.peak_hold.iter().zip(frame.bins_dbfs.iter()) {
            assert!(peak >= smoothed);
        }
    }

    #[test]
    fn a_finite_level_recovers_a_non_finite_bin() {
        let state = Arc::new(Mutex::new(SdrMetrics::fixture()));
        let mut accumulator = SpectrumAccumulator::default();
        accumulator
            .publish(
                &state,
                PowerTrace {
                    frequencies_hz: vec![100_000_000, 101_000_000],
                    levels_dbm: vec![f32::NAN, -90.0],
                    rbw_hz: None,
                },
            )
            .unwrap();
        accumulator
            .publish(
                &state,
                PowerTrace {
                    frequencies_hz: vec![100_000_000, 101_000_000],
                    levels_dbm: vec![-70.0, -80.0],
                    rbw_hz: None,
                },
            )
            .unwrap();

        let metrics = state.lock().unwrap_or_else(|error| error.into_inner());
        let frame = metrics.waterfall.last_fft.as_ref().unwrap();
        assert_eq!(frame.bins_dbfs[0], -70.0);
        assert!(frame.bins_dbfs[1].is_finite());
        assert!(frame.peak_hold[0] >= frame.bins_dbfs[0]);
    }

    #[test]
    fn non_finite_initial_levels_publish_as_unavailable() {
        let state = Arc::new(Mutex::new(SdrMetrics::fixture()));
        let mut accumulator = SpectrumAccumulator::default();
        accumulator
            .publish(
                &state,
                PowerTrace {
                    frequencies_hz: vec![100_000_000, 101_000_000],
                    levels_dbm: vec![f32::NAN, f32::INFINITY],
                    rbw_hz: None,
                },
            )
            .unwrap();

        let metrics = state.lock().unwrap_or_else(|error| error.into_inner());
        let frame = metrics.waterfall.last_fft.as_ref().unwrap();
        assert_eq!(
            frame.bins_dbfs.as_slice(),
            &[f32::NEG_INFINITY, f32::NEG_INFINITY]
        );
        assert_eq!(frame.peak_hold.as_slice(), frame.bins_dbfs.as_slice());
        assert_eq!(frame.noise_floor, f32::NEG_INFINITY);
    }

    #[test]
    fn an_rbw_change_resets_smoothing() {
        let state = Arc::new(Mutex::new(SdrMetrics::fixture()));
        let mut accumulator = SpectrumAccumulator::default();
        for (rbw_hz, levels) in [(Some(10_000), -90.0), (Some(20_000), -40.0)] {
            accumulator
                .publish(
                    &state,
                    PowerTrace {
                        frequencies_hz: vec![100_000_000, 101_000_000],
                        levels_dbm: vec![levels; 2],
                        rbw_hz,
                    },
                )
                .unwrap();
        }
        let metrics = state.lock().unwrap_or_else(|error| error.into_inner());
        let frame = metrics.waterfall.last_fft.as_ref().unwrap();
        assert_eq!(frame.bins_dbfs.as_slice(), &[-40.0, -40.0]);
        assert_eq!(frame.peak_hold.as_slice(), &[-40.0, -40.0]);
        assert_eq!(frame.enbw_hz, 20_000.0);
    }

    #[test]
    fn rejected_traces_are_reported_at_a_bounded_rate() {
        let state = Arc::new(Mutex::new(SdrMetrics::fixture()));
        let mut reporter = RejectionReporter::default();
        let now = Instant::now();
        reporter.report(&state, TraceRejection::Empty, now);
        reporter.report(
            &state,
            TraceRejection::LengthMismatch,
            now + Duration::from_secs(1),
        );
        reporter.report(
            &state,
            TraceRejection::NonUniformGrid,
            now + REJECTION_REPORT_INTERVAL,
        );

        let metrics = state.lock().unwrap_or_else(|error| error.into_inner());
        assert_eq!(metrics.ui.log.len(), 2);
        assert!(metrics.ui.log[0].text.contains("empty trace"));
        assert!(metrics.ui.log[1]
            .text
            .contains("1 additional trace rejected"));
    }

    #[test]
    fn invalid_traces_return_specific_rejections() {
        let state = Arc::new(Mutex::new(SdrMetrics::fixture()));
        let mut accumulator = SpectrumAccumulator::default();
        let cases = [
            (
                PowerTrace {
                    frequencies_hz: vec![],
                    levels_dbm: vec![],
                    rbw_hz: None,
                },
                TraceRejection::Empty,
            ),
            (
                PowerTrace {
                    frequencies_hz: vec![100, 200],
                    levels_dbm: vec![-90.0],
                    rbw_hz: None,
                },
                TraceRejection::LengthMismatch,
            ),
            (
                PowerTrace {
                    frequencies_hz: vec![100, 200, 350],
                    levels_dbm: vec![-90.0; 3],
                    rbw_hz: None,
                },
                TraceRejection::NonUniformGrid,
            ),
        ];
        for (trace, expected) in cases {
            assert_eq!(accumulator.publish(&state, trace), Err(expected));
        }
    }

    #[test]
    fn trace_window_uses_the_measured_edges() {
        assert_eq!(
            trace_window(&[100_000, 200_000, 300_000]),
            Some((200_000, 200_000.0))
        );
        assert_eq!(trace_window(&[100_000]), None);
        assert_eq!(trace_window(&[100_000, 200_000, 350_000]), None);
        assert_eq!(trace_window(&[300_000, 200_000, 100_000]), None);
        assert_eq!(
            trace_window(&[100_000, 133_333, 166_667, 200_000]),
            Some((150_000, 100_000.0))
        );
    }
}
