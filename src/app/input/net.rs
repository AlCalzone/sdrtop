// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MusiThang <viktor.laszlo92@protonmail.com>

//! The NET section's panel handlers.

use crossterm::event::{KeyCode, KeyEvent};

use super::{global, metrics, InputCtx, KeyAction};

/// The census table: move the cursor, change what orders it.
///
/// **The cursor moves through the ordering, not through the census**, which is
/// why this asks the panel's own ordering for the addresses rather than the
/// order they happen to be stored in. `CensusState` then remembers the device
/// rather than the row, so a re-sort under the cursor leaves it where it was.
pub(super) fn net_census(key: KeyEvent, ctx: &mut InputCtx<'_>) -> KeyAction {
    let mut m = metrics(ctx.state);
    // Nothing decodes an address yet, so the ordering is empty and the cursor
    // has nowhere to go. The keys still work: they change what the table *would*
    // be ordered by, which is what the chrome tag shows.
    let ordered: Vec<[u8; 6]> = Vec::new();
    match key.code {
        KeyCode::Up => m.net.census.move_cursor(&ordered, -1),
        KeyCode::Down => m.net.census.move_cursor(&ordered, 1),
        KeyCode::Char('s') => m.net.census.cycle_sort(),
        KeyCode::Char('r') => m.net.census.reverse(),
        _ => {
            drop(m);
            return global::handle(key, ctx);
        }
    }
    KeyAction::Continue
}
