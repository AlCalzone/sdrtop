// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MusiThang <viktor.laszlo92@protonmail.com>

//! `NetState` - what the 2.4 GHz receiver is doing right now.
//!
//! Deliberately small, and it will stay smaller than it looks like it should.
//! What belongs here is the state a *header* has to read, because that is the
//! one thing every panel in the section shares: the rest lives with the panel
//! that produced it. See `dev_docs/net-foundation-design.md` section 9.2.

/// Whether the receiver is walking the band or sitting on one channel.
///
/// **This changes what every number below it means**, which is why it is the
/// first field in the header and why it is never absent. A duty cycle measured
/// while sweeping is a sample of a channel; measured while locked it is that
/// channel's whole story. Design section 13.1 makes every panel say which one it
/// is looking at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum NetMode {
    /// Stepping across the band. The default, because nothing has been chosen
    /// yet and a receiver that has not been told where to sit is surveying.
    #[default]
    Survey,
    /// Parked on one channel.
    Lock,
}

impl NetMode {
    /// The word the header shows. Upper case because it is a mode, not a
    /// reading, and the eye has to find it without looking.
    pub fn label(self) -> &'static str {
        match self {
            NetMode::Survey => "SURVEY",
            NetMode::Lock => "LOCK",
        }
    }

    /// The chrome tag every panel in the section carries.
    ///
    /// A panel says *which claim its numbers are*, and the engine spells and
    /// colours it, which is the rule for every tag. Design section 13.1: the
    /// mode is part of the reading, so this is not optional for a panel here and
    /// `every_net_panel_says_how_its_numbers_were_gathered` is what makes that
    /// true rather than customary.
    pub fn tag(self) -> crate::ui::panel::Tag {
        match self {
            NetMode::Survey => crate::ui::panel::Tag::Survey,
            NetMode::Lock => crate::ui::panel::Tag::Lock,
        }
    }

    pub fn toggled(self) -> Self {
        match self {
            NetMode::Survey => NetMode::Lock,
            NetMode::Lock => NetMode::Survey,
        }
    }
}

/// Where the radio belongs once the survey gives the tuner back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NetExit {
    pub tune_hz: u64,
    /// Whether the radio stays where the pass left it, rather than going back
    /// where the survey found it.
    pub locked: bool,
}

#[derive(Clone, Debug, Default)]
pub struct NetState {
    pub mode: NetMode,
    pub health: NetDecodeHealth,
    pub band: BandOccupancy,
    /// The tuning the survey interrupted, so it can be given back.
    ///
    /// **In the state rather than in the task**, for the reason
    /// [`crate::state::SweepState::end`] gives about the same field: the task is
    /// not the only thing that has to put the radio back. Quitting mid-pass
    /// never reaches another iteration of that loop - the process ends - and
    /// `save_config` would write out whichever hop the survey was parked on, so
    /// the app would reopen somewhere in the middle of the band, one position
    /// further along each time.
    pub pre_survey_hz: Option<u64>,
}

impl NetState {
    /// Give the tuner back, and say where the radio belongs.
    ///
    /// **The two ways out of a survey want opposite answers, and treating them
    /// as one was a bug.** Leaving the section ends the survey, so the radio goes
    /// back where it was found. Switching to lock means "stay here": the user
    /// pressed the key while looking at a position, and that position is what
    /// they meant. The first version restored in both cases, while the key
    /// handler logged `NET locked to <the current hop>` - so the log said one
    /// thing and the radio did another.
    ///
    /// `tuned_hz` is `radio.frequency`. Safe on a state that never surveyed, and
    /// safe to call twice: the second call has nothing left to take and answers
    /// with whatever the caller wrote back after the first.
    pub fn end(&mut self, tuned_hz: u64) -> NetExit {
        let pre = self.pre_survey_hz.take();
        if self.mode == NetMode::Lock {
            NetExit {
                tune_hz: tuned_hz,
                locked: true,
            }
        } else {
            NetExit {
                tune_hz: pre.unwrap_or(tuned_hz),
                locked: false,
            }
        }
    }
}

/// What the receiver missed, and what it never had a chance to see.
///
/// Design section 13.2: this is not a debug panel, it is testimony. Without it
/// every count in the section is a lower bound presented as a total, because the
/// three ways a sample can go missing are all invisible from downstream. The
/// driver drops them before the block is stamped; the bounded feed refuses whole
/// blocks under load and carries no record that it did; and a run broken in the
/// middle takes whatever was being assembled with it.
///
/// Every field here is a count of something that happened, not a rate and not a
/// judgement. The panel does the dividing.
#[derive(Clone, Debug, Default)]
pub struct NetDecodeHealth {
    /// Blocks that reached the worker.
    pub blocks_in: u64,
    /// I/Q pairs in them.
    pub pairs_in: u64,
    /// Times the stream was interrupted: a block that did not continue the one
    /// before it, for either reason.
    ///
    /// **The first block of a run is not one of these.** Opening the section
    /// mid-stream means the first block to arrive is thousands of sequence
    /// numbers past nothing, and counting that as an interruption would put a
    /// phantom gap on the panel every time the user looked at it.
    pub gaps: u64,
    /// A floor on the blocks that went missing, summed over those gaps.
    ///
    /// A floor because the driver says samples went, never how many: one is the
    /// smallest number that is certainly not an overstatement, and zero would
    /// leave the panel calling a lossy link healthy. Same rule as
    /// `BlockPlan::dropped`, which is where this comes from.
    pub blocks_lost: u64,
    /// Unbroken blocks since the last gap.
    pub run_blocks: u64,
    /// Blocks the bounded feed refused in the last poll window, and since the
    /// radio was opened.
    pub refused: u64,
    pub refused_session: u64,
    /// The deepest the feed queue got in the last window.
    pub peak_depth: u64,
    /// When the last block arrived. `None` before the first one.
    pub last_block: Option<std::time::Instant>,
}

/// One megahertz of the band, as measured over the last dwell.
///
/// A cell that was never inside the observed span has `windows` of zero, and
/// that is the difference between "nothing was transmitting here" and "nobody
/// looked here". The panel draws them differently, because they are different
/// answers and rule 2 is about exactly this.
#[derive(Clone, Copy, Debug, Default)]
pub struct CellReading {
    /// Transform windows this cell was measured over.
    pub windows: u64,
    /// Fraction of them with something in this cell, with the false-alarm floor
    /// removed. Zero is a measurement.
    pub duty: f64,
    /// Mean and peak power in the cell, relative to the converter's full scale.
    pub mean_dbfs: f64,
    pub peak_dbfs: f64,
    /// What fraction of wall time this cell was actually under observation,
    /// between its last two measurements.
    ///
    /// About one in [`crate::signal::net::survey::Plan::hops`] while surveying,
    /// and one while locked. `None` until a cell has been measured twice, which
    /// is when the question first has an answer.
    ///
    /// **The duty cycle is not scaled by this**, and the temptation to is worth
    /// naming: a channel busy all the time, seen for a sixth of the time, is
    /// busy all the time. Scaling its reading to sixteen percent would not be a
    /// sampled measurement, it would be a wrong one. What sampling costs is
    /// certainty, not magnitude, and that is carried by the window count and
    /// reported as an uncertainty.
    pub coverage: Option<f64>,
    /// When this cell was last measured.
    pub measured: Option<std::time::Instant>,
}

impl CellReading {
    /// Whether anybody looked here.
    pub fn observed(&self) -> bool {
        self.windows > 0
    }
}

/// The band as last measured, one megahertz at a time.
#[derive(Clone, Debug, Default)]
pub struct BandOccupancy {
    /// Empty until the first dwell completes; `occupancy::CELLS` long after.
    pub cells: Vec<CellReading>,
    /// The receiver's own noise floor, which every duty cycle here was measured
    /// against. `None` before the first dwell.
    pub noise_dbfs: Option<f64>,
    /// Whether the floor's preconditions held. When they did not, nothing here
    /// is a measurement and the panel says so rather than drawing it.
    pub trusted: bool,
    /// What the plane looked like, kept because it is the reason `trusted` is
    /// what it is and a panel that only showed the verdict would be asking to be
    /// believed.
    pub tail: f64,
    pub spread: f64,
    /// How long one transform window was. The resolution every duty cycle here
    /// was measured at, and what turns a window count back into seconds.
    pub window_s: f64,
}

impl BandOccupancy {
    /// Fold one dwell into the band, keeping every cell the dwell did not see.
    ///
    /// **This is what makes a survey a survey.** Each dwell measures the slice
    /// the radio was pointed at; the rest of the band keeps what the last pass
    /// found there, with the time it was found. A dwell that replaced the whole
    /// band would leave a receiver seeing a fifth of it reporting the other four
    /// fifths as unobserved on every frame, which is a picture of the receiver
    /// rather than of the band.
    ///
    /// The floor is the receiver's rather than the position's, so the newest one
    /// wins outright: it is a fact about the front end at this gain, and the
    /// front end does not change between hops.
    pub fn absorb(&mut self, dwell: BandOccupancy, now: std::time::Instant) {
        if self.cells.len() != dwell.cells.len() {
            self.cells = vec![CellReading::default(); dwell.cells.len()];
        }
        for (old, new) in self.cells.iter_mut().zip(dwell.cells.iter()) {
            if new.windows == 0 {
                continue;
            }
            let observed_s = new.windows as f64 * dwell.window_s;
            // Coverage needs two measurements to be a fraction of anything, and
            // the honest answer before that is that nobody knows yet.
            let coverage = old.measured.map(|then| {
                let elapsed = now.duration_since(then).as_secs_f64();
                if elapsed > 0.0 {
                    (observed_s / elapsed).min(1.0)
                } else {
                    1.0
                }
            });
            *old = CellReading {
                coverage,
                measured: Some(now),
                ..*new
            };
        }
        self.noise_dbfs = dwell.noise_dbfs;
        self.trusted = dwell.trusted;
        self.tail = dwell.tail;
        self.spread = dwell.spread;
        self.window_s = dwell.window_s;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// **Locking means "stay here". Leaving the section means "put it back".**
    ///
    /// Treating the two as one exit was the bug: the survey restored the
    /// pre-survey tuning on both, while the key handler logged
    /// `NET locked to <the current hop>`. The log said one thing and the radio
    /// did another, and the user pressed the key precisely because of what they
    /// were looking at.
    #[test]
    fn locking_keeps_the_position_and_leaving_gives_it_back() {
        // A survey started at 2412 and is currently parked on 2442.
        let mut net = NetState {
            pre_survey_hz: Some(2_412_000_000),
            ..Default::default()
        };

        // Locked: the position the pass is on is the one the user meant.
        net.mode = NetMode::Lock;
        let exit = net.end(2_442_000_000);
        assert_eq!(exit.tune_hz, 2_442_000_000);
        assert!(exit.locked);

        // Still surveying and leaving the section: back where it was found.
        let mut net = NetState {
            pre_survey_hz: Some(2_412_000_000),
            ..Default::default()
        };
        let exit = net.end(2_442_000_000);
        assert_eq!(exit.tune_hz, 2_412_000_000);
        assert!(!exit.locked);
    }

    /// Safe on a state that never surveyed, and safe to call twice, because the
    /// quit path calls it unconditionally and the task may have called it first.
    #[test]
    fn ending_a_survey_that_never_ran_leaves_the_radio_alone() {
        let mut net = NetState::default();
        assert_eq!(net.end(2_437_000_000).tune_hz, 2_437_000_000);

        let mut net = NetState {
            pre_survey_hz: Some(2_412_000_000),
            ..Default::default()
        };
        assert_eq!(net.end(2_442_000_000).tune_hz, 2_412_000_000);
        // The task wrote the answer back; quitting must not move it again.
        assert_eq!(net.end(2_412_000_000).tune_hz, 2_412_000_000);
        assert_eq!(net.pre_survey_hz, None);
    }

    /// Leaving the section after locking does not undo the lock.
    ///
    /// The lock already took the interrupted tuning, so there is nothing left to
    /// restore and the radio stays where the user put it - which is what they
    /// asked for and is why `take` rather than a read is the right call.
    #[test]
    fn leaving_the_section_does_not_undo_a_lock() {
        let mut net = NetState {
            pre_survey_hz: Some(2_412_000_000),
            ..Default::default()
        };
        net.mode = NetMode::Lock;
        assert_eq!(net.end(2_442_000_000).tune_hz, 2_442_000_000);
        // Now the user leaves NET entirely, still locked.
        assert_eq!(net.end(2_442_000_000).tune_hz, 2_442_000_000);
    }

    /// One dwell's worth of band: `cells` measured, the rest untouched.
    fn dwell(cells: &[(usize, f64, u64)]) -> BandOccupancy {
        let mut out = BandOccupancy {
            cells: vec![CellReading::default(); 83],
            noise_dbfs: Some(-78.0),
            trusted: true,
            tail: 2.1,
            spread: 30.0,
            window_s: 6.4e-6,
        };
        for &(c, duty, windows) in cells {
            out.cells[c] = CellReading {
                windows,
                duty,
                mean_dbfs: -60.0,
                peak_dbfs: -30.0,
                coverage: None,
                measured: None,
            };
        }
        out
    }

    /// **This is what makes a survey a survey.** Each dwell sees one slice; the
    /// band keeps what the last pass found everywhere else.
    ///
    /// Without it, a receiver seeing a fifth of the band would report the other
    /// four fifths as unobserved on every frame, which is a picture of the
    /// receiver rather than of the band, and the panel's whole
    /// observed-versus-unobserved distinction would collapse to "wherever the
    /// radio happens to be pointed this instant".
    #[test]
    fn a_dwell_folds_into_the_band_rather_than_replacing_it() {
        let t0 = Instant::now();
        let mut band = BandOccupancy::default();

        band.absorb(dwell(&[(10, 0.4, 8_000)]), t0);
        assert_eq!(band.cells[10].duty, 0.4);
        assert!(band.cells[10].observed());

        // A second dwell somewhere else does not take the first one with it.
        band.absorb(dwell(&[(60, 0.9, 8_000)]), t0 + Duration::from_millis(300));
        assert_eq!(band.cells[60].duty, 0.9);
        assert_eq!(
            band.cells[10].duty, 0.4,
            "the other end of the band is still what the last pass found"
        );
        assert!(band.cells[10].observed());
        // And a cell no pass has reached yet is still unobserved, which is a
        // different answer from empty.
        assert!(!band.cells[30].observed());
    }

    /// Sampling costs certainty, not magnitude.
    ///
    /// A channel busy all the time, watched a sixth of the time, is busy all the
    /// time. Scaling its reading to sixteen percent would not be a sampled
    /// measurement, it would be a wrong one - and it is the obvious thing to
    /// write, which is why it is asserted against.
    #[test]
    fn the_coverage_is_reported_and_never_multiplied_into_the_duty_cycle() {
        let t0 = Instant::now();
        let mut band = BandOccupancy::default();

        // A saturated cell, measured over 8000 windows of 6.4 us: 51 ms of
        // looking.
        band.absorb(dwell(&[(10, 1.0, 8_000)]), t0);
        assert_eq!(band.cells[10].duty, 1.0);
        assert_eq!(
            band.cells[10].coverage, None,
            "one measurement is not a fraction of anything yet"
        );

        // A pass later - 625 ms - the same cell is measured again.
        band.absorb(dwell(&[(10, 1.0, 8_000)]), t0 + Duration::from_millis(625));
        assert_eq!(band.cells[10].duty, 1.0, "still busy all the time");
        let coverage = band.cells[10].coverage.expect("two measurements");
        assert!(
            (coverage - 0.0819).abs() < 0.001,
            "51 ms of looking in 625: {coverage}"
        );
    }

    /// Locked, the receiver is looking almost all the time, and the coverage
    /// says so rather than being pinned to one by the mode.
    #[test]
    fn locking_shows_as_coverage_rather_than_being_assumed() {
        let t0 = Instant::now();
        let mut band = BandOccupancy::default();
        band.absorb(dwell(&[(10, 0.3, 8_000)]), t0);
        band.absorb(dwell(&[(10, 0.3, 8_000)]), t0 + Duration::from_millis(52));
        let coverage = band.cells[10].coverage.unwrap();
        assert!(coverage > 0.95, "{coverage}");
        // It never exceeds one, however the clock lands.
        band.absorb(dwell(&[(10, 0.3, 8_000)]), t0 + Duration::from_millis(52));
        assert!(band.cells[10].coverage.unwrap() <= 1.0);
    }

    /// The floor is the receiver's, not the position's, so the newest wins.
    #[test]
    fn the_newest_floor_is_the_bands_floor() {
        let t0 = Instant::now();
        let mut band = BandOccupancy::default();
        band.absorb(dwell(&[(10, 0.3, 8_000)]), t0);
        let mut second = dwell(&[(60, 0.3, 8_000)]);
        second.noise_dbfs = Some(-71.0);
        second.trusted = false;
        band.absorb(second, t0 + Duration::from_millis(300));
        assert_eq!(band.noise_dbfs, Some(-71.0));
        assert!(!band.trusted, "a front end on its rails is on its rails");
    }
}
