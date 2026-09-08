// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MusiThang <viktor.laszlo92@protonmail.com>

//! `net_survey_task` - walks the 2.4 GHz band while the NET section is open and
//! the mode is survey.
//!
//! The same shape as [`super::sweep`] and for the same reasons: retune, settle,
//! dwell, advance, and steer nothing else. It runs no DSP of its own - the
//! worker in `signal::net` is already measuring whatever the radio is pointed
//! at, so this task's whole job is deciding where that is.
//!
//! **The plan is not here.** Where to point and for how long is
//! [`crate::signal::net::survey::Plan`], which is plain arithmetic over plain
//! data and is asserted with no radio. What is left in this file is the part
//! that cannot be tested that way: the device calls, the sleeps, and putting the
//! tuning back afterwards.
//!
//! It cannot fight the frequency sweep over the tuner, because the two are in
//! different sections and only one section is on screen at a time. It can fight
//! the user, who may retune while a pass is running; the pass wins until the
//! mode is switched to lock, which is what lock is for.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::hardware::SdrDevice;
use crate::signal::net::survey::{Plan, DWELL, SETTLE};
use crate::state::{NetMode, SdrMetrics};

/// How often to look again when there is nothing to do.
const IDLE_POLL: Duration = Duration::from_millis(100);

pub fn spawn_net_survey_task(state: Arc<Mutex<SdrMetrics>>, device: Arc<dyn SdrDevice>) {
    tokio::spawn(async move {
        let mut surveying = false;
        // Alternate passes walk one cell along, so no megahertz stays in a
        // position's DC shadow. See `Plan::DODGE_HZ`.
        let mut pass = 0u64;
        // Said once, not once a poll.
        let mut refused = false;

        loop {
            let (active, span_hz, rate_hz, tuned) = {
                let m = state.lock().unwrap_or_else(|e| e.into_inner());
                let span = if m.radio.bb_filter_hz > 0 {
                    (m.radio.bb_filter_hz as f64).min(m.radio.config_sample_rate)
                } else {
                    m.radio.config_sample_rate
                };
                (
                    m.ui.is_net_section() && m.net.mode == NetMode::Survey && m.radio.hw_streaming,
                    span,
                    m.radio.config_sample_rate,
                    m.radio.frequency,
                )
            };

            if !active {
                // Where the radio belongs is `NetState::end`'s answer, not this
                // task's: locking means stay here, leaving the section means go
                // back where the survey found you, and quitting mid-pass has to
                // reach the same answer without another iteration of this loop.
                if surveying {
                    surveying = false;
                    let exit = {
                        let mut m = state.lock().unwrap_or_else(|e| e.into_inner());
                        m.net.end(tuned)
                    };
                    let _ = device.set_frequency(exit.tune_hz);
                    let mut m = state.lock().unwrap_or_else(|e| e.into_inner());
                    m.radio.frequency = exit.tune_hz;
                    m.push_log(if exit.locked {
                        format!("NET locked to {:.3} MHz", exit.tune_hz as f64 / 1e6)
                    } else {
                        format!(
                            "NET survey stopped, back to {:.3} MHz",
                            exit.tune_hz as f64 / 1e6
                        )
                    });
                }
                tokio::time::sleep(IDLE_POLL).await;
                continue;
            }

            let plan = Plan::for_span(span_hz, pass);
            let bins = crate::signal::net::scan::bins_for(rate_hz);
            // A receiver too narrow to see past its own oscillator has positions
            // to visit and nothing to learn at any of them. The gate asked
            // whether this radio can receive the cheapest mode; whether it can
            // survey a band is a different question, and this is where it is
            // answered.
            if plan.hops.is_empty() || !plan.covers_anything(rate_hz, bins) {
                if !refused {
                    refused = true;
                    let mut m = state.lock().unwrap_or_else(|e| e.into_inner());
                    m.push_log(format!(
                        "NET survey: {:.1} MHz of view is too narrow to measure past the \
                         local oscillator; lock to a channel instead",
                        span_hz / 1e6
                    ));
                }
                tokio::time::sleep(IDLE_POLL).await;
                continue;
            }
            refused = false;
            if !surveying {
                surveying = true;
                let mut m = state.lock().unwrap_or_else(|e| e.into_inner());
                m.net.pre_survey_hz = Some(tuned);
                // The coverage is logged rather than assumed. A plan is only a
                // plan if its positions between them see the whole band, and a
                // radio whose usable span left a gap would otherwise report that
                // stretch as unobserved for ever with nothing saying why.
                let covered = plan.covered(rate_hz, bins).iter().filter(|c| **c).count();
                m.push_log(format!(
                    "NET survey: {} positions of {:.1} MHz, {} ms a pass, {covered} of {} MHz covered",
                    plan.hops.len(),
                    span_hz / 1e6,
                    plan.cycle().as_millis(),
                    crate::signal::net::occupancy::CELLS
                ));
            }

            for hz in &plan.hops {
                {
                    let m = state.lock().unwrap_or_else(|e| e.into_inner());
                    if !m.ui.is_net_section() || m.net.mode != NetMode::Survey {
                        break;
                    }
                }
                let _ = device.set_frequency(*hz);
                {
                    // The worker reads `radio.frequency` to know which cells it
                    // is looking at, so this has to be the position and not the
                    // user's last tuning, and it has to be set before the dwell
                    // rather than after it.
                    let mut m = state.lock().unwrap_or_else(|e| e.into_inner());
                    m.radio.frequency = *hz;
                }
                tokio::time::sleep(SETTLE + DWELL).await;
            }
            pass = pass.wrapping_add(1);
        }
    });
}
