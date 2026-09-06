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
        let mut resume_hz = 0u64;

        loop {
            let (active, span_hz, tuned) = {
                let m = state.lock().unwrap_or_else(|e| e.into_inner());
                let span = if m.radio.bb_filter_hz > 0 {
                    (m.radio.bb_filter_hz as f64).min(m.radio.config_sample_rate)
                } else {
                    m.radio.config_sample_rate
                };
                (
                    m.ui.is_net_section() && m.net.mode == NetMode::Survey && m.radio.hw_streaming,
                    span,
                    m.radio.frequency,
                )
            };

            if !active {
                // Leaving survey puts the radio back where the user left it.
                // The pass owns the tuner while it runs, so without this a user
                // who switched to lock would be locked to whichever position the
                // sweep happened to stop on.
                if surveying {
                    surveying = false;
                    let _ = device.set_frequency(resume_hz);
                    let mut m = state.lock().unwrap_or_else(|e| e.into_inner());
                    m.radio.frequency = resume_hz;
                    m.push_log(format!(
                        "NET survey stopped, back to {:.3} MHz",
                        resume_hz as f64 / 1e6
                    ));
                }
                tokio::time::sleep(IDLE_POLL).await;
                continue;
            }

            let plan = Plan::for_span(span_hz);
            if plan.hops.is_empty() {
                tokio::time::sleep(IDLE_POLL).await;
                continue;
            }
            if !surveying {
                surveying = true;
                resume_hz = tuned;
                let mut m = state.lock().unwrap_or_else(|e| e.into_inner());
                // The coverage is logged rather than assumed. A plan is only a
                // plan if its positions between them see the whole band, and a
                // radio whose usable span left a gap would otherwise report that
                // stretch as unobserved for ever with nothing saying why.
                let covered = plan.covered().iter().filter(|c| **c).count();
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
        }
    });
}
