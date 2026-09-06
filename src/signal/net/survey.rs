// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MusiThang <viktor.laszlo92@protonmail.com>

//! Where to point the radio, and for how long, to see the whole band.
//!
//! A receiver that reaches this section sees eighteen or twenty megahertz of an
//! eighty-three megahertz band, so the only way to survey it is to hop. Design
//! section 13.1 makes the consequence part of the reading rather than a footnote:
//! **a duty-cycle-sampled census and a complete capture are different claims**,
//! and every number gathered this way is marked with how it was gathered.
//!
//! Everything here is plain arithmetic over plain data, so the plan can be
//! asserted with no radio anywhere. The task that executes it only steers the
//! tuner and waits.

use std::time::Duration;

use super::{band, occupancy};

/// Samples discarded after a retune while the PLL settles.
///
/// The same figure the frequency sweep uses, from `state::SWEEP_SETTLING_MS`,
/// because it is a fact about the radio rather than about either feature. Two
/// numbers here would be two chances to disagree about the same PLL.
pub const SETTLE: Duration = Duration::from_millis(crate::state::SWEEP_SETTLING_MS);

/// How long to sit on one position.
///
/// Twice the dwell the scan needs to publish a measurement, because the feed is
/// lossy: fifty milliseconds of *observation* takes more than fifty milliseconds
/// of wall clock whenever a block is dropped, and a hop that moved on at exactly
/// fifty would publish nothing on the positions where it mattered most.
pub const DWELL: Duration = Duration::from_millis(100);

/// One pass across the band.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    /// Centre frequencies, low to high.
    pub hops: Vec<u64>,
    /// What each of them sees.
    pub span_hz: f64,
}

impl Plan {
    /// The positions needed to cover the band with a receiver seeing `span_hz`.
    ///
    /// **Derived from the radio, not chosen.** The number of positions is the
    /// fewest whose spans cover the band, and they are spread evenly across it
    /// so the overlap is shared rather than piled up at one end. Centres are
    /// rounded to whole megahertz to line up with the cell grid, which the
    /// overlap absorbs: rounding by at most half a megahertz cannot open a gap
    /// in a plan whose steps are already shorter than its spans.
    pub fn for_span(span_hz: f64) -> Self {
        let width = (band::HIGH_HZ - band::LOW_HZ) as f64;
        if !span_hz.is_finite() || span_hz <= 0.0 {
            return Self {
                hops: Vec::new(),
                span_hz: 0.0,
            };
        }
        // One position is enough for a receiver that can see the whole band, and
        // for one that cannot see a whole cell there is nothing to plan.
        let n = (width / span_hz).ceil().max(1.0) as usize;
        let first = band::LOW_HZ as f64 + span_hz / 2.0;
        let step = if n > 1 {
            (width - span_hz) / (n - 1) as f64
        } else {
            0.0
        };
        let hops = (0..n)
            .map(|k| {
                let hz = first + k as f64 * step;
                (hz / 1e6).round() as u64 * 1_000_000
            })
            .collect();
        Self { hops, span_hz }
    }

    /// How long one pass takes.
    pub fn cycle(&self) -> Duration {
        (SETTLE + DWELL) * self.hops.len() as u32
    }

    /// Every cell any position of this plan observes.
    ///
    /// The plan is only a plan if this is the whole band; `the_plan_covers_the_whole_band`
    /// is the assertion, and it is the one that matters, because a gap here is a
    /// stretch of spectrum the survey would report as unobserved for ever
    /// without anything saying why.
    pub fn covered(&self) -> Vec<bool> {
        let mut seen = vec![false; occupancy::CELLS];
        for hz in &self.hops {
            for c in occupancy::cells_observed(*hz as f64, self.span_hz) {
                seen[c] = true;
            }
        }
        seen
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one that matters. A gap in the plan is a stretch of band the survey
    /// would report as unobserved for ever, with nothing on screen saying why.
    #[test]
    fn the_plan_covers_the_whole_band() {
        for span in [
            2_000_000.0,
            8_000_000.0,
            10_000_000.0,
            18_000_000.0,
            20_000_000.0,
            40_000_000.0,
            100_000_000.0,
        ] {
            let plan = Plan::for_span(span);
            let covered = plan.covered();
            let missed: Vec<usize> = (0..occupancy::CELLS).filter(|c| !covered[*c]).collect();
            assert!(
                missed.is_empty(),
                "span {span}: {} hops missed cells {missed:?}",
                plan.hops.len()
            );
        }
    }

    /// The hop count is the fewest that can work, and the positions are spread
    /// rather than piled up at one end.
    #[test]
    fn the_plan_is_the_fewest_positions_that_cover_the_band() {
        // 83.5 MHz of band, 18 MHz at a time, is five.
        let plan = Plan::for_span(18_000_000.0);
        assert_eq!(plan.hops.len(), 5);
        assert_eq!(
            plan.hops,
            vec![
                2_409_000_000,
                2_425_000_000,
                2_442_000_000,
                2_458_000_000,
                2_475_000_000
            ]
        );
        // Evenly spread: no two steps differ by more than the megahertz the
        // rounding can move a centre.
        let steps: Vec<u64> = plan.hops.windows(2).map(|w| w[1] - w[0]).collect();
        let (lo, hi) = (steps.iter().min().unwrap(), steps.iter().max().unwrap());
        assert!(hi - lo <= 1_000_000, "{steps:?}");

        // A radio that sees the whole band does not hop at all.
        assert_eq!(Plan::for_span(100_000_000.0).hops.len(), 1);
        // And a nonsense span is no plan rather than a division by zero.
        assert!(Plan::for_span(0.0).hops.is_empty());
        assert!(Plan::for_span(f64::NAN).hops.is_empty());
    }

    /// Every position is inside what the gate guarantees the radio can tune to.
    #[test]
    fn every_position_is_one_the_gate_admits() {
        for span in [8_000_000.0, 18_000_000.0, 20_000_000.0] {
            for hz in Plan::for_span(span).hops {
                assert!(
                    (super::super::gate::LOWEST_CENTRE_HZ..=super::super::gate::HIGHEST_CENTRE_HZ)
                        .contains(&hz),
                    "span {span} would tune to {hz}"
                );
            }
        }
    }

    /// A pass is what its parts add up to, and the cycle time is what sets how
    /// much of the band any one reading covers.
    #[test]
    fn a_pass_is_the_settle_and_the_dwell_at_every_position() {
        let plan = Plan::for_span(18_000_000.0);
        assert_eq!(plan.cycle(), (SETTLE + DWELL) * 5);
        assert_eq!(plan.cycle(), Duration::from_millis(625));
        // Which is the coverage every cell in the section is reported at: one
        // dwell in every pass.
        let coverage = DWELL.as_secs_f64() / plan.cycle().as_secs_f64();
        assert!((coverage - 0.16).abs() < 0.01, "{coverage}");
    }
}
