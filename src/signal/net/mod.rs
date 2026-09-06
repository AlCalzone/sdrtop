// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MusiThang <viktor.laszlo92@protonmail.com>

//! The NET feature's own layer: what is true of the 2.4 GHz band and of the
//! radio pointed at it, above any one protocol.
//!
//! Dependencies run one way. `dsp` knows no protocol; the protocol arcs know
//! only `dsp`; this module sits above them and aggregates. It is also where the
//! questions live that no single protocol can answer, and [`gate`] is the first
//! of them: **can this radio do any of this at all?**

pub mod gate;
