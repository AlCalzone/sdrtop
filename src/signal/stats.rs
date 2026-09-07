// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MusiThang <viktor.laszlo92@protonmail.com>

pub(crate) fn finite_ema(previous: f32, sample: f32, alpha: f32) -> f32 {
    match (previous.is_finite(), sample.is_finite()) {
        (true, true) => alpha * sample + (1.0 - alpha) * previous,
        (false, true) => sample,
        (true, false) => previous,
        (false, false) => f32::NEG_INFINITY,
    }
}

pub(crate) fn average_and_peak(
    samples: &[f32],
    smoothed: &mut [f32],
    peak: &mut [f32],
    alpha: f32,
    decay_db: f32,
    has_history: bool,
) {
    for ((smoothed, peak), sample) in smoothed
        .iter_mut()
        .zip(peak.iter_mut())
        .zip(samples.iter().copied())
    {
        if !has_history {
            *smoothed = f32::NEG_INFINITY;
            *peak = f32::NEG_INFINITY;
        }
        *smoothed = finite_ema(*smoothed, sample, alpha);
        let decayed_peak = if peak.is_finite() {
            *peak - decay_db
        } else {
            f32::NEG_INFINITY
        };
        *peak = if smoothed.is_finite() {
            decayed_peak.max(*smoothed)
        } else {
            decayed_peak
        };
    }
}

pub(crate) fn quietest_mean(values: &[f32], scratch: &mut [f32], fraction: usize) -> f32 {
    let mut finite_count = 0;
    for value in values.iter().copied().filter(|value| value.is_finite()) {
        scratch[finite_count] = value;
        finite_count += 1;
    }
    if finite_count == 0 {
        return f32::NEG_INFINITY;
    }

    let count = (finite_count / fraction.max(1)).max(1);
    scratch[..finite_count].select_nth_unstable_by(count - 1, f32::total_cmp);
    scratch[..count].iter().sum::<f32>() / count as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finite_ema_keeps_the_available_value() {
        assert_eq!(finite_ema(-90.0, -80.0, 0.2), -88.0);
        assert_eq!(finite_ema(f32::NAN, -80.0, 0.2), -80.0);
        assert_eq!(finite_ema(-90.0, f32::NAN, 0.2), -90.0);
        assert_eq!(finite_ema(f32::NAN, f32::INFINITY, 0.2), f32::NEG_INFINITY);
    }

    #[test]
    fn average_and_peak_handles_missing_samples() {
        let mut smoothed = [0.0, 0.0];
        let mut peak = [0.0, 0.0];
        average_and_peak(
            &[f32::NAN, -90.0],
            &mut smoothed,
            &mut peak,
            0.2,
            0.5,
            false,
        );
        average_and_peak(&[-70.0, f32::NAN], &mut smoothed, &mut peak, 0.2, 0.5, true);
        assert_eq!(smoothed, [-70.0, -90.0]);
        assert_eq!(peak, [-70.0, -90.0]);
    }

    #[test]
    fn quietest_mean_ignores_non_finite_values() {
        let values = [-100.0, f32::NAN, -90.0, f32::INFINITY, -80.0];
        let mut scratch = vec![0.0; values.len()];
        assert_eq!(quietest_mean(&values, &mut scratch, 10), -100.0);
    }
}
