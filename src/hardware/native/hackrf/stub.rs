// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MusiThang <viktor.laszlo92@protonmail.com>

use crate::hardware::{
    AcquisitionModel, Boost, DeliveryModel, DeviceCapabilities, DeviceListing, GainModel,
    LevelUnit, SampleFormat, SampleGeometry, StageSpec,
};
use crate::state::{DEFAULT_FREQUENCY, DEFAULT_SAMPLE_RATE, HACKRF_SAMPLES_PER_TRANSFER};

pub fn list() -> Vec<DeviceListing> {
    Vec::new()
}

pub fn gain_model() -> GainModel {
    GainModel::new(
        vec![
            StageSpec::ranged("LNA", 0.0, 40.0, 8.0),
            StageSpec::ranged("VGA", 0.0, 62.0, 2.0),
        ],
        "LNA",
        "LNA",
    )
    .with_second_stage()
    .with_boost(Boost::Element(StageSpec::ranged("AMP", 0.0, 14.0, 14.0)))
    .with_chain_diagram("LNA\u{25b8}MIX\u{25b8}VGA")
    .with_no_cascade_reason("no cascade")
}

pub fn caps() -> DeviceCapabilities {
    DeviceCapabilities {
        acquisition: AcquisitionModel::IqSamples,
        level_unit: LevelUnit::Dbfs,
        level_min_db: -120.0,
        level_max_db: 0.0,
        trace_stale_ms: 500,
        freq_min_hz: 1_000_000,
        freq_max_hz: 6_000_000_000,
        sample_rate_min_hz: 2_000_000.0,
        sample_rate_max_hz: 20_000_000.0,
        default_frequency_hz: DEFAULT_FREQUENCY,
        default_sample_rate_hz: DEFAULT_SAMPLE_RATE,
        sample_geometry: SampleGeometry {
            format: SampleFormat::Int8,
            full_scale: 128.0,
        },
        gain: gain_model(),
        samples_per_transfer: HACKRF_SAMPLES_PER_TRANSFER,
        has_bb_filter: true,
        friis_applicable: true,
        delivery: DeliveryModel::Push,
    }
}

pub fn board_rev_name(rev: u8) -> &'static str {
    match rev {
        0 => "HackRF One (old)",
        6 => "HackRF One r6",
        7 => "HackRF One r7",
        8 => "HackRF One r8",
        9 => "HackRF One r9",
        10 => "HackRF One r10",
        0xFE => "Undetected",
        0xFF => "Unrecognized",
        _ => "Unknown",
    }
}

pub fn compute_bb_filter_bw(sample_rate_hz: f64) -> u32 {
    const STEPS: &[u32] = &[
        1_750_000, 2_500_000, 3_500_000, 5_000_000, 5_500_000, 6_000_000, 7_000_000, 8_000_000,
        9_000_000, 10_000_000, 12_000_000, 14_000_000, 15_000_000, 20_000_000, 24_000_000,
        28_000_000,
    ];
    let target = sample_rate_hz as u32;
    STEPS
        .iter()
        .copied()
        .min_by_key(|&bandwidth| (bandwidth as i64 - target as i64).unsigned_abs())
        .unwrap_or(10_000_000)
}
