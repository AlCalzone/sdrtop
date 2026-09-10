// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MusiThang <viktor.laszlo92@protonmail.com>

//! `FREQUENCY REFERENCE` - what our own oscillator is doing, and what that makes
//! every ppm reading in the app worth.
//!
//! Design section 7.3 puts this card on the RF bench because that is where a
//! user already goes to ask about their receiver. It is not a NET card: the
//! error it reports is in every ppm number the app prints, and hiding it inside
//! one feature would make it a caveat repeated in several places and believed in
//! none.
//!
//! **The card's job is to say which of three things a ppm reading is**, and only
//! then what the number was. Unreferenced readings are relative and useful -
//! ranking the clocks in a room needs no reference at all - but they are not
//! absolute, and the difference is the whole card.

use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};

use crate::signal::dsp::uncertainty::Uncertain;
use crate::signal::reference::STANDARDS;
use crate::state::{Provenance, SdrMetrics, REFERENCE_STALE_S};
use crate::ui::chrome::section;
use crate::ui::widgets::reading::Reading;

use super::super::rf_bench::{row, Row};

/// Label column for this card.
///
/// Wider than the panel's `LABEL_W` because these rows are named rather than
/// coded, and `row` treats `label_w` as a *minimum*: a longer label is not
/// truncated, it silently pushes the right-hand reading off the end of the line.
/// `noise.rs` carries the same constant for the same reason, and this card was
/// written with the panel's narrow one and walked straight into it.
const LABEL_W: usize = 8;

/// The smallest ppm difference this reading is meant to resolve.
///
/// A tenth of a part per million. Below that the reference is not worth applying
/// to anything: the crystals this app meets are tens of ppm out, and a
/// correction whose own uncertainty swamps a tenth of one is a correction that
/// changes no decision. It is what [`Reading`] is handed, so a reference too
/// coarse to matter dashes rather than printing digits it has not earned.
const RESOLUTION_PPM: f64 = 0.1;

fn colour(p: &Provenance, theme: &crate::Theme) -> Color {
    match p {
        // Not a warning. Relative readings are correct and useful; they are
        // simply a different claim, and the label is what carries that.
        Provenance::Unreferenced => theme.label,
        Provenance::Referenced => theme.value,
        Provenance::Traceable => theme.status_ok,
    }
}

pub(super) fn draw(
    lines: &mut Vec<Line<'static>>,
    state: &SdrMetrics,
    iw: usize,
    theme: &crate::Theme,
) {
    let now = std::time::Instant::now();
    lines.push(section("FREQUENCY REFERENCE", "", iw, theme));

    let Some(r) = state.radio.reference.as_ref() else {
        // Nothing established, and the card says what that costs and what to do
        // about it rather than showing a blank or a zero.
        lines.push(row(
            Row {
                label: "STATUS",
                label_w: LABEL_W,
                mid: Provenance::Unreferenced.label().to_string(),
                mid_col: theme.label,
                right: "ppm is relative only".to_string(),
                right_col: theme.label,
            },
            iw,
            theme,
        ));
        lines.push(hint(iw, theme));
        return;
    };

    let effective = r.effective(now);
    let stale = r.is_stale(now);
    lines.push(row(
        Row {
            label: "STATUS",
            label_w: LABEL_W,
            mid: effective.label().to_string(),
            mid_col: colour(&effective, theme),
            right: if stale {
                // Named, not hinted at: a reference that expired is a thing the
                // user did that stopped being true, and the fix is to do it
                // again.
                format!("was {}", r.provenance.label())
            } else {
                r.source.clone()
            },
            right_col: if stale { theme.stale } else { theme.label },
        },
        iw,
        theme,
    ));

    // The measurement itself, drawn through idiom A so it looks like every other
    // reading in the app and dashes when its uncertainty cannot support it.
    let reading = Reading::new(
        Uncertain::from_sigma(r.ppm, r.sigma_ppm),
        "ppm",
        RESOLUTION_PPM,
    );
    lines.push(row(
        Row {
            label: "LO ERROR",
            label_w: LABEL_W,
            mid: reading.text(),
            mid_col: if stale { theme.stale } else { theme.value },
            right: age(r.age(now), stale),
            right_col: if stale { theme.stale } else { theme.label },
        },
        iw,
        theme,
    ));

    if stale {
        lines.push(hint(iw, theme));
    }
}

/// `4 min ago`, and what it is measured against.
fn age(age: std::time::Duration, stale: bool) -> String {
    let secs = age.as_secs();
    let when = if secs < 90 {
        format!("{secs} s ago")
    } else {
        format!("{} min ago", secs / 60)
    };
    if stale {
        format!("expired at {} min", REFERENCE_STALE_S / 60)
    } else {
        when
    }
}

/// What to do about it, in the space the numbers would have filled.
///
/// The range comes from the table rather than being typed here, so a station
/// added or a frequency corrected cannot leave this sentence describing the old
/// list.
fn hint(iw: usize, theme: &crate::Theme) -> Line<'static> {
    let mhz = |s: Option<&crate::signal::reference::Standard>| s.map_or(0.0, |s| s.hz as f64 / 1e6);
    let text = format!(
        "tune {:.1}-{:.0} MHz (WWV), then press [Y]",
        mhz(STANDARDS.first()),
        mhz(STANDARDS.last()),
    );
    Line::from(vec![
        Span::raw(" "),
        Span::styled(text, Style::default().fg(theme.label)),
        Span::raw(" ".repeat(iw.saturating_sub(40).max(1))),
    ])
}

#[cfg(test)]
mod tests {
    use crate::state::fixture::draw;
    use crate::state::{FrequencyReference, Provenance, SdrMetrics, REFERENCE_STALE_S};
    use crate::ui::panels::lab::rf_chain::RfChainPanel;
    use std::time::{Duration, Instant};

    fn with(reference: Option<FrequencyReference>) -> SdrMetrics {
        let mut m = SdrMetrics::fixture().streaming();
        m.radio.reference = reference;
        m
    }

    fn traceable(age: Duration) -> FrequencyReference {
        FrequencyReference {
            ppm: -2.37,
            sigma_ppm: 0.08,
            provenance: Provenance::Traceable,
            source: "WWV 10 MHz".to_string(),
            at: Instant::now() - age,
        }
    }

    fn card(m: &SdrMetrics) -> String {
        draw(RfChainPanel, 70, 40, m)
            .into_iter()
            .skip_while(|l| !l.contains("FREQUENCY REFERENCE"))
            .take(4)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// With no reference the card says what that costs and what to do, rather
    /// than showing a blank or a zero. A zero would be a measurement.
    #[test]
    fn an_unreferenced_radio_says_so_and_says_what_to_do() {
        let out = card(&with(None));
        assert!(out.contains("RELATIVE"), "{out}");
        assert!(out.contains("relative only"), "{out}");
        assert!(out.contains("press [Y]"), "{out}");
        assert!(
            !out.contains("LO ERROR"),
            "there is no reading to show: {out}"
        );
        assert!(!out.contains("0.0"), "{out}");
    }

    #[test]
    fn a_fresh_reference_reads_out_with_its_source_and_its_age() {
        let out = card(&with(Some(traceable(Duration::from_secs(240)))));
        assert!(out.contains("TRACEABLE"), "{out}");
        assert!(out.contains("WWV 10 MHz"), "{out}");
        assert!(out.contains("-2.37 ±0.08 ppm"), "{out}");
        assert!(out.contains("4 min ago"), "{out}");
        assert!(!out.contains("press [Y]"), "nothing to fix: {out}");
    }

    /// **The one that matters.** A stale reference is still on screen, because
    /// the user should see what expired, but its status is what it is worth now
    /// and not what it was worth when it was taken.
    #[test]
    fn a_stale_reference_reads_as_relative_and_says_what_it_was() {
        let out = card(&with(Some(traceable(Duration::from_secs(
            REFERENCE_STALE_S + 60,
        )))));
        assert!(out.contains("RELATIVE"), "{out}");
        assert!(!out.contains("STATUS TRACEABLE"), "{out}");
        // What it was, so the user knows what they are being asked to redo.
        assert!(out.contains("was TRACEABLE"), "{out}");
        assert!(out.contains("expired at 15 min"), "{out}");
        assert!(out.contains("press [Y]"), "{out}");
    }

    /// A reference too coarse to be worth applying dashes, rather than printing
    /// digits it has not earned. Idiom A's rule, on this panel.
    #[test]
    fn a_reference_that_cannot_resolve_a_tenth_of_a_ppm_dashes() {
        let mut r = traceable(Duration::from_secs(10));
        r.sigma_ppm = 4.0;
        let out = card(&with(Some(r)));
        assert!(
            out.contains("±4"),
            "the spread is still worth saying: {out}"
        );
        assert!(!out.contains("-2.37"), "{out}");
        assert!(out.contains('—'), "{out}");
    }

    /// Whatever the width, no row runs past the frame.
    #[test]
    fn the_card_fits_every_width_the_bench_gets() {
        for w in 44..100u16 {
            for m in [with(None), with(Some(traceable(Duration::from_secs(10))))] {
                for line in draw(RfChainPanel, w, 40, &m) {
                    assert!(line.chars().count() <= w as usize, "{w}: {line:?}");
                }
            }
        }
    }
}
