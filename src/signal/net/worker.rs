// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MusiThang <viktor.laszlo92@protonmail.com>

//! The 2.4 GHz worker: one thread, one block at a time, and at this point no
//! decoder behind it.
//!
//! Same shape as [`crate::signal::DemodWorker`], for the same reason: a thread
//! that owns the state carried between blocks, so that everything below it can
//! be a pure function of its arguments and be tested with no radio anywhere.
//!
//! **What it does at N13 is count, and that is the point.** Design section 13.2
//! makes what the receiver missed a first-class displayed number rather than an
//! inference, and a feed whose losses are only visible once there is something
//! to lose is a feed nobody will trust when the losses matter. The counting is
//! built and shown first; the detectors arrive at N14 and the decoders after
//! them. Nothing here reports a burst, a preamble or a frame, and the panel says
//! so rather than printing a zero that would read as "we looked".

use std::sync::{Arc, Mutex};
use std::time::Instant;

use crossbeam_channel::Receiver;

use crate::hardware::{SampleGeometry, StreamBlock};
use crate::signal::stream::plan_block;
use crate::state::SdrMetrics;

pub struct NetWorker {
    pub sample_rx: Receiver<StreamBlock>,
    pub state: Arc<Mutex<SdrMetrics>>,
    pub geometry: SampleGeometry,
}

/// What the worker carries from one block to the next.
///
/// Three fields, and two of them exist only so that a gap can be told from a
/// pause. See [`Run::suspend`].
#[derive(Default)]
struct Run {
    last_seq: u64,
    /// The sequence of the previous block *that reached us*, or `None` when the
    /// run has not started.
    drop_ref: Option<u64>,
    /// Unbroken blocks since the last gap.
    blocks: u64,
}

impl Run {
    /// The section was closed, so the feed stopped forwarding.
    ///
    /// The device goes on counting every callback, so the next block to arrive
    /// will be thousands of sequence numbers away without a single one having
    /// been lost. Clearing `drop_ref` is what stops that jump being reported as
    /// the worst loss event of the session. The run length goes with it: there
    /// is no run any more.
    fn suspend(&mut self) {
        self.drop_ref = None;
        self.blocks = 0;
    }
}

impl NetWorker {
    pub fn new(
        sample_rx: Receiver<StreamBlock>,
        state: Arc<Mutex<SdrMetrics>>,
        geometry: SampleGeometry,
    ) -> Self {
        Self {
            sample_rx,
            state,
            geometry,
        }
    }

    pub fn run(self) {
        let mut run = Run::default();
        let pair_bytes = self.geometry.bytes_per_pair() as u64;

        while let Ok(StreamBlock {
            seq,
            gap_before,
            bytes,
        }) = self.sample_rx.recv()
        {
            let started = run.drop_ref.is_some();
            let plan = plan_block(seq, gap_before, run.last_seq, run.drop_ref);
            // The clock is read outside the lock, because the lock block below
            // does integer work only and a float or a syscall inside one is a
            // dropped frame on the UI thread.
            let now = Instant::now();
            run.last_seq = seq;
            run.drop_ref = Some(seq);

            let pairs = bytes.len() as u64 / pair_bytes.max(1);
            // **A run has to have started before it can be interrupted.**
            // `plan_block` guards its `dropped` count with `drop_ref` and does
            // not guard `contiguous` with anything, because for the demod the
            // difference is invisible: a first block declared discontiguous just
            // resets session state that is already empty. Here the same flag is
            // about to become a number on a panel, and the section is normally
            // opened on a radio that has been streaming for a minute - so the
            // first block through carries a sequence number thousands past
            // whatever this worker last saw, and would report an interruption
            // that never happened, once per visit.
            let broke = started && !plan.contiguous;
            run.blocks = if broke || !started { 1 } else { run.blocks + 1 };

            let still_open = {
                let mut m = self.state.lock().unwrap_or_else(|e| e.into_inner());
                let h = &mut m.net.health;
                h.blocks_in = h.blocks_in.saturating_add(1);
                h.pairs_in = h.pairs_in.saturating_add(pairs);
                h.gaps = h.gaps.saturating_add(u64::from(broke));
                h.blocks_lost = h.blocks_lost.saturating_add(plan.dropped);
                h.run_blocks = run.blocks;
                h.last_block = Some(now);
                m.ui.is_net_section()
            };

            // Closing the section stops `process_block` forwarding, but blocks
            // already in the channel still arrive - and the run they belong to
            // is over whether or not they are the last of it.
            if !still_open {
                run.suspend();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::SampleFormat;

    fn eight_bit() -> SampleGeometry {
        SampleGeometry {
            format: SampleFormat::Int8,
            full_scale: 128.0,
        }
    }

    /// Run the worker over a scripted feed and read back what it recorded.
    ///
    /// The channel is closed before the worker starts, so `run` drains it and
    /// returns rather than blocking: the loop under test is the same one either
    /// way, and a test that has to join a live thread is a test that can hang
    /// CI.
    fn feed(blocks: &[(u64, bool, usize)], open: bool) -> crate::state::NetDecodeHealth {
        let mut m = SdrMetrics::fixture();
        m.ui.section = if open {
            crate::signal::net::SECTION.to_string()
        } else {
            String::new()
        };
        let state = Arc::new(Mutex::new(m));
        let (tx, rx) = crossbeam_channel::unbounded();
        for &(seq, gap_before, pairs) in blocks {
            tx.send(StreamBlock {
                seq,
                gap_before,
                bytes: vec![0u8; pairs * 2],
            })
            .unwrap();
        }
        drop(tx);
        NetWorker::new(rx, Arc::clone(&state), eight_bit()).run();
        let m = state.lock().unwrap();
        m.net.health.clone()
    }

    #[test]
    fn an_unbroken_feed_reports_no_gaps_and_one_growing_run() {
        let h = feed(&[(1, false, 64), (2, false, 64), (3, false, 64)], true);
        assert_eq!(h.blocks_in, 3);
        assert_eq!(h.pairs_in, 192, "eight-bit pairs are two bytes each");
        assert_eq!((h.gaps, h.blocks_lost), (0, 0));
        assert_eq!(h.run_blocks, 3);
        assert!(h.last_block.is_some());
    }

    /// Opening the section on a radio that is already streaming is the normal
    /// case, and it must not read as a loss.
    ///
    /// The device stamps every callback whether or not this feed is being
    /// forwarded to, so the first block through carries a sequence number
    /// thousands past whatever the worker last saw. Counting that as an
    /// interruption put a phantom gap on the panel once per visit, and a panel
    /// whose job is to say what the receiver missed is the last place that can
    /// afford one.
    #[test]
    fn opening_the_section_mid_stream_is_not_an_interruption() {
        let h = feed(&[(9_412, false, 64), (9_413, false, 64)], true);
        assert_eq!(h.gaps, 0, "nothing was interrupted; nothing had started");
        assert_eq!(h.blocks_lost, 0);
        assert_eq!(
            h.run_blocks, 2,
            "and the run is the two blocks that arrived"
        );
    }

    #[test]
    fn a_gap_breaks_the_run_and_starts_a_new_one() {
        // Three good blocks, then the driver says samples went missing.
        let h = feed(
            &[
                (1, false, 64),
                (2, false, 64),
                (3, false, 64),
                (4, true, 64),
            ],
            true,
        );
        assert_eq!(h.gaps, 1);
        assert_eq!(h.blocks_lost, 1, "the floor: samples went, count unknown");
        assert_eq!(h.run_blocks, 1, "and the new run is one block long");
    }

    #[test]
    fn blocks_lost_by_the_channel_are_counted_too() {
        // 1, then 5: three blocks the bounded feed refused, plus a driver gap.
        let h = feed(&[(1, false, 64), (5, true, 64)], true);
        assert_eq!(h.blocks_lost, 4);
        assert_eq!(h.gaps, 1);
    }

    #[test]
    fn closing_the_section_ends_the_run_rather_than_losing_it() {
        // The section is closed, so every block in the channel is one already in
        // flight when it closed. The jump between them must not be reported as a
        // loss, because the device counts callbacks whether or not we take them.
        let h = feed(&[(1, false, 64), (9_000, false, 64)], false);
        assert_eq!(h.blocks_in, 2, "blocks in flight are still counted");
        assert_eq!(
            (h.gaps, h.blocks_lost),
            (0, 0),
            "and the pause is not a loss"
        );
    }
}
