// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MusiThang <viktor.laszlo92@protonmail.com>

use crate::hardware::{
    AcquisitionModel, Boost, DeliveryModel, DeviceCapabilities, DeviceListing, GainModel,
    LevelUnit, SampleFormat, SampleGeometry, StageSpec,
};

pub fn list() -> Vec<DeviceListing> {
    Vec::new()
}

pub fn gain_model(steps_db: &[u32]) -> GainModel {
    let stages = if steps_db.is_empty() {
        Vec::new()
    } else {
        vec![StageSpec::tabled(
            "Tuner",
            steps_db.iter().map(|&gain| gain as f64).collect(),
        )]
    };
    GainModel::new(stages, "Tuner", "TUN")
        .with_boost(Boost::GainMode)
        .with_chain_diagram("TUNER")
        .with_no_cascade_reason("single tuner, no cascade")
        .with_gauge_fallback(49)
}

pub fn observer_caps() -> DeviceCapabilities {
    DeviceCapabilities {
        acquisition: AcquisitionModel::IqSamples,
        level_unit: LevelUnit::Dbfs,
        level_min_db: -120.0,
        level_max_db: 0.0,
        trace_stale_ms: 500,
        freq_min_hz: 24_000_000,
        freq_max_hz: 1_766_000_000,
        sample_rate_min_hz: 900_001.0,
        sample_rate_max_hz: 3_200_000.0,
        default_frequency_hz: 100_000_000,
        default_sample_rate_hz: 2_400_000.0,
        sample_geometry: SampleGeometry {
            format: SampleFormat::Uint8,
            full_scale: 128.0,
        },
        gain: gain_model(&[0]),
        samples_per_transfer: 32_768,
        has_bb_filter: false,
        friis_applicable: false,
        delivery: DeliveryModel::Push,
    }
}
