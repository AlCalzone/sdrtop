// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MusiThang <viktor.laszlo92@protonmail.com>

//! Who is here: the population of the band, keyed by address.
//!
//! Design section 10 puts this in `net` rather than in either arc, and the
//! reason is the one that shapes the whole module: **a device is a device
//! whichever protocol found it.** A Wi-Fi station and a Bluetooth peripheral are
//! the same kind of row - an address, when it was last heard, how much it has
//! said, how strongly - and the panel that ranks them must not care which arc
//! filled it in.
//!
//! **Nothing fills this yet.** The record and the ordering are here so that the
//! table above them can be built and tested before either arc exists; design
//! section 1.1's address display switch (`full`, `oui`, `masked`) arrives with
//! the arc that has addresses to switch between.

use std::time::Instant;

/// One transmitter, as the census knows it.
#[derive(Clone, Debug)]
#[allow(dead_code)] // filled by the first arc that decodes an address
pub struct Device {
    /// The MAC or BD_ADDR, as transmitted.
    pub address: [u8; 6],
    /// Packets attributed to it.
    pub packets: u64,
    /// The strongest it has been heard.
    pub best_rssi_dbm: f32,
    /// When it was last heard.
    pub last_seen: Instant,
}

impl Device {
    /// `a4:83:e7:1c:09:be`, the form design section 1.1 makes the default.
    #[allow(dead_code)] // filled by the first arc that decodes an address
    pub fn address_text(&self) -> String {
        self.address
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":")
    }
}

/// What the table can be ordered by, in the order the columns are drawn.
///
/// The names are the column titles, so the chrome tag and the header cannot
/// disagree about what the table is sorted by.
pub const SORT_KEYS: &[&str] = &["ADDRESS", "SEEN", "PKTS", "RSSI"];

/// Put `devices` in the order the panel asked for.
///
/// **Total, and deterministic where the key ties.** Two devices heard the same
/// number of times must not swap places between frames, so the address breaks
/// every tie: it is the one field that is unique by definition.
#[allow(dead_code)] // called by the panel once an arc fills the census
pub fn order(devices: &mut [Device], sort: usize, descending: bool, now: Instant) {
    devices.sort_by(|a, b| {
        let key = match sort {
            1 => now
                .saturating_duration_since(a.last_seen)
                .cmp(&now.saturating_duration_since(b.last_seen)),
            2 => a.packets.cmp(&b.packets),
            3 => a.best_rssi_dbm.total_cmp(&b.best_rssi_dbm),
            _ => std::cmp::Ordering::Equal,
        };
        let key = if descending { key.reverse() } else { key };
        key.then_with(|| a.address.cmp(&b.address))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn device(last: u8, packets: u64, rssi: f32, ago_s: u64, now: Instant) -> Device {
        Device {
            address: [0xa4, 0x83, 0xe7, 0x1c, 0x09, last],
            packets,
            best_rssi_dbm: rssi,
            last_seen: now - Duration::from_secs(ago_s),
        }
    }

    #[test]
    fn an_address_reads_as_an_address() {
        let now = Instant::now();
        assert_eq!(
            device(0xbe, 0, 0.0, 0, now).address_text(),
            "a4:83:e7:1c:09:be"
        );
        assert_eq!(
            Device {
                address: [0, 0, 0, 0, 0, 0],
                ..device(0, 0, 0.0, 0, now)
            }
            .address_text(),
            "00:00:00:00:00:00"
        );
    }

    #[test]
    fn every_column_orders_by_the_thing_it_names() {
        let now = Instant::now();
        // (address tail, packets, rssi, seconds ago)
        let make = || {
            vec![
                device(0x03, 10, -80.0, 30, now),
                device(0x01, 500, -41.0, 2, now),
                device(0x02, 7, -60.0, 90, now),
            ]
        };
        let tails = |d: &[Device]| d.iter().map(|x| x.address[5]).collect::<Vec<_>>();

        let mut d = make();
        order(&mut d, 0, false, now);
        assert_eq!(tails(&d), vec![1, 2, 3], "by address, ascending");

        let mut d = make();
        order(&mut d, 1, false, now);
        assert_eq!(tails(&d), vec![1, 3, 2], "most recently seen first");

        let mut d = make();
        order(&mut d, 2, true, now);
        assert_eq!(tails(&d), vec![1, 3, 2], "busiest first");

        let mut d = make();
        order(&mut d, 3, true, now);
        assert_eq!(tails(&d), vec![1, 2, 3], "strongest first");
    }

    /// **A tie must not shuffle.** Two devices with the same count would
    /// otherwise swap places between frames, and a table whose rows move under
    /// the cursor for no reason is one nobody can use.
    #[test]
    fn a_tie_is_broken_by_the_one_field_that_cannot_tie() {
        let now = Instant::now();
        let mut a = vec![
            device(0x09, 42, -50.0, 5, now),
            device(0x02, 42, -50.0, 5, now),
            device(0x07, 42, -50.0, 5, now),
        ];
        let mut b = a.clone();
        b.reverse();
        order(&mut a, 2, true, now);
        order(&mut b, 2, true, now);
        let tails = |d: &[Device]| d.iter().map(|x| x.address[5]).collect::<Vec<_>>();
        assert_eq!(
            tails(&a),
            tails(&b),
            "the same rows in a different order in"
        );
        assert_eq!(tails(&a), vec![2, 7, 9]);
    }

    /// The keys and the columns are one list, so the chrome tag and the header
    /// cannot name different things.
    #[test]
    fn the_sort_keys_are_the_column_titles() {
        assert_eq!(SORT_KEYS.len(), 4);
        assert!(SORT_KEYS.contains(&"PKTS"));
    }
}
