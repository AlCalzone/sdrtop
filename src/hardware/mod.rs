// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MusiThang <viktor.laszlo92@protonmail.com>

//! The hardware layer, and the map of it.
//!
//! [`native`] holds the radios sdrtop drives itself. [`soapy`] holds everything
//! reachable through libSoapySDR. [`tinysa`] holds swept spectrum analyzers.
//! [`discovery`] is the only module that sees all three groups.
//!
//! The rest is shared and backend-neutral: [`traits`] is the vocabulary,
//! [`process`] the per-sample decode all three feed, [`gain`] the placement
//! policy, [`sysfs`] the read-only USB scan behind observer mode.
//!
//! **What this file re-exports is what the rest of the app may know about the
//! hardware**, so nothing backend-specific belongs in the list below. A fact
//! about one radio is reached through the module that owns it, spelled out at
//! the call site: `native::hackrf::board_rev_name` says whose board revision it
//! is in a way that `hardware::board_rev_name` did not.

pub mod discovery;
pub mod gain;
pub mod native;
pub mod process;
pub mod soapy;
pub mod sysfs;
pub mod tinysa;
mod traits;

pub use discovery::{list_all_devices, open_device, DeviceKind, DeviceListing};
#[cfg(test)]
pub(crate) use traits::RateSet;
pub use traits::{
    AcquisitionKind, Boost, DeliveryModel, DeviceCapabilities, DeviceInfo, DeviceOption,
    DirectSweepConfig, FeedHealth, GainModel, LevelUnit, PowerTrace, PowerTraceTarget, RxContext,
    SampleFormat, SampleGeometry, SdrDevice, SoftwareStack, StageSpec, StreamBlock,
    IQ_TRACE_STALE_MS,
};

pub(crate) fn sanitize_device_options(
    options: Vec<DeviceOption>,
) -> (Vec<DeviceOption>, Vec<String>) {
    let mut valid = Vec::with_capacity(options.len());
    let mut notes = Vec::new();
    for mut option in options {
        let name = if option.label.is_empty() {
            option.id.as_str()
        } else {
            option.label.as_str()
        };
        let Some(first) = option.choices.first().cloned() else {
            notes.push(format!(
                "Warning: device option '{name}' has no choices. Hiding it."
            ));
            continue;
        };
        if !option.choices.contains(&option.selected_choice) {
            notes.push(format!(
                "Warning: device option '{name}' selected unavailable choice '{}'. Using '{first}'.",
                option.selected_choice
            ));
            option.selected_choice = first;
        }
        valid.push(option);
    }
    (valid, notes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn option(choices: &[&str], selected: &str) -> DeviceOption {
        DeviceOption {
            id: "bandwidth".into(),
            label: "Bandwidth".into(),
            choices: choices.iter().map(|choice| (*choice).into()).collect(),
            selected_choice: selected.into(),
        }
    }

    #[test]
    fn options_without_choices_are_hidden_and_reported() {
        let (options, notes) = sanitize_device_options(vec![option(&[], "")]);

        assert!(options.is_empty());
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("Bandwidth"));
        assert!(notes[0].contains("no choices"));
    }

    #[test]
    fn an_unavailable_selected_choice_uses_the_first_choice() {
        let (options, notes) =
            sanitize_device_options(vec![option(&["Narrow", "Wide"], "Missing")]);

        assert_eq!(options[0].selected_choice, "Narrow");
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("unavailable choice"));
    }
}
