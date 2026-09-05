// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MusiThang <viktor.laszlo92@protonmail.com>

use std::fs;
use std::path::{Path, PathBuf};

use crate::hardware::{DeviceKind, DeviceListing};

pub(super) fn list() -> Vec<DeviceListing> {
    list_at(Path::new("/sys/class/tty"), Path::new("/dev"))
}

fn list_at(tty_root: &Path, dev_root: &Path) -> Vec<DeviceListing> {
    let Ok(entries) = fs::read_dir(tty_root) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("ttyACM") {
            continue;
        }
        let Ok(mut ancestor) = fs::canonicalize(entry.path()) else {
            continue;
        };
        loop {
            if let Some(product) = matching_product(&ancestor) {
                let path = dev_root.join(name.as_ref());
                found.push((path, product));
                break;
            }
            if !ancestor.pop() {
                break;
            }
        }
    }
    found.sort_by(|left, right| left.0.cmp(&right.0));
    found
        .into_iter()
        .enumerate()
        .map(|(index, (path, product))| DeviceListing {
            kind: DeviceKind::TinySa,
            index,
            label: format!("{product} · {}", path.display()),
            serial: None,
            args: None,
            path: Some(path),
        })
        .collect()
}

fn matching_product(path: &Path) -> Option<String> {
    let vendor = read(path.join("idVendor"))?;
    let product_id = read(path.join("idProduct"))?;
    if !vendor.eq_ignore_ascii_case("0483") || !product_id.eq_ignore_ascii_case("5740") {
        return None;
    }
    let manufacturer = read(path.join("manufacturer")).unwrap_or_default();
    let product = read(path.join("product")).unwrap_or_default();
    let known_manufacturer = manufacturer.eq_ignore_ascii_case("tinysa.org");
    let known_product =
        product.eq_ignore_ascii_case("tinysa") || product.eq_ignore_ascii_case("tinysa4");
    if !known_manufacturer && !known_product {
        return None;
    }
    Some(if product.is_empty() {
        "tinySA".to_string()
    } else {
        product
    })
}

fn read(path: PathBuf) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_string())
}
