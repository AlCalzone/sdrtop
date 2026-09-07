// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MusiThang <viktor.laszlo92@protonmail.com>

//! The hardware layer, and the map of it.
//!
//! [`native`] holds the radios sdrtop drives itself. [`soapy`] holds everything
//! reached through libSoapySDR. [`tinysa`] holds swept spectrum analyzers.
//! [`discovery`] is the only module that sees all three groups.

pub mod discovery;
pub mod gain;
pub mod native;
pub mod process;
pub mod soapy;
pub mod sysfs;
pub mod tinysa;
mod traits;

pub use discovery::{list_all_devices, open_device, DeviceKind, DeviceListing};
pub use traits::{
    AcquisitionModel, Boost, DeliveryModel, DeviceCapabilities, DeviceInfo, FeedHealth, GainModel,
    LevelUnit, PowerTrace, RxContext, SampleFormat, SampleGeometry, SdrDevice, SoftwareStack,
    StageSpec, StreamBlock,
};
