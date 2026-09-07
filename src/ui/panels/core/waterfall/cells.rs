// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MusiThang <viktor.laszlo92@protonmail.com>

//! The waterfall grid itself: history rows painted as coloured half-blocks.
//!
//! Each character cell carries **two** time rows - `▀` with the older row as the
//! background and the newer as the foreground - so the visible history is twice
//! the panel's height. That doubling is why the scroll and stride arithmetic
//! keeps two units in play at once, `skip_chars` and `skip_data`, and mixing
//! them up scrolls at half or double speed.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use ratatui::{
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::palette::{magnitude_to_color_palette, ColorDepth, WaterfallPalette};
use crate::state::BinAxis;

/// Top of the colour scale. The waterfall is always referenced to full scale;
/// only the floor (`db_min`) moves, under `↑`/`↓`.
pub(super) const DB_MAX: f32 = 0.0;

/// Max dB over the bin range `[start, end)` of one waterfall row, clamped to the
/// row's own length. Rows are normally all the (fixed) FFT bin count, but reading
/// each row against its own length means a row that ever differs - e.g. if the FFT
/// size changed at runtime - degrades to a partial read instead of slicing out of
/// bounds and panicking. An empty or fully out-of-range span reads −∞ (the floor).
pub(super) fn band_max(row: &[f32], start: usize, end: usize) -> f32 {
    let s = start.min(row.len());
    let e = end.min(row.len()).max(s);
    if s == e {
        f32::NEG_INFINITY
    } else {
        row[s..e].iter().copied().fold(f32::NEG_INFINITY, f32::max)
    }
}

/// Which bins each plot column covers, under the current frequency zoom.
///
/// Zoom keeps the centre `1/zoom` of the row's bins - the same slice the bonded
/// spectrum above narrows to, so `+`/`-` zoom both plots as one instrument.
pub(super) struct Columns {
    lo_bin: usize,
    visible_n: usize,
    row_bins: usize,
    cols: usize,
    bin_axis: BinAxis,
}

impl Columns {
    pub fn new(
        row_bins: usize,
        first_bin: usize,
        visible_n: usize,
        cols: usize,
        bin_axis: BinAxis,
    ) -> Self {
        let row_bins = row_bins.max(1);
        let visible_n = visible_n.max(1).min(row_bins);
        Self {
            lo_bin: first_bin.min(row_bins - visible_n),
            visible_n,
            row_bins,
            cols: cols.max(1),
            bin_axis,
        }
    }

    /// The `[start, end)` bin span column `col` reads. Always non-empty, so a
    /// wide panel over few bins still gets one bin per column rather than none.
    pub fn range(&self, col: usize) -> (usize, usize) {
        let (start, end) = match self.bin_axis {
            BinAxis::FftBins => (
                col * self.visible_n / self.cols,
                (col + 1) * self.visible_n / self.cols,
            ),
        };
        let start = (self.lo_bin + start).min(self.row_bins - 1);
        let end = (self.lo_bin + end).max(start + 1).min(self.row_bins);
        (start, end)
    }
}

/// Paint the history grid. `skip_data` is in *data rows*, not character rows.
#[allow(clippy::too_many_arguments)]
pub(super) fn draw(
    f: &mut Frame,
    area: Rect,
    rows: &VecDeque<(Instant, Arc<Vec<f32>>)>,
    columns: &Columns,
    cursor_col: Option<usize>,
    skip_data: usize,
    db_min: f32,
    palette: WaterfallPalette,
    theme: &crate::Theme,
) {
    let cols = area.width as usize;
    let depth = ColorDepth::detect();
    let color = |db: f32| magnitude_to_color_palette(db, db_min, DB_MAX, depth, theme, palette);
    let floor = color(f32::NEG_INFINITY);

    let mut lines: Vec<Line> = Vec::with_capacity(area.height as usize);
    let mut data = rows.iter().skip(skip_data).take(area.height as usize * 2);
    while let Some((_ts, top_row)) = data.next() {
        let bot_row = data.next().map(|(_ts, r)| r.as_ref());
        let spans: Vec<Span> = (0..cols)
            .map(|col| {
                let (lo, hi) = columns.range(col);
                let bot_color = bot_row.map(|r| color(band_max(r, lo, hi))).unwrap_or(floor);
                // The cursor column keeps the background so the history still reads
                // through it, but takes a bright foreground as its marker.
                let top_color = if Some(col) == cursor_col {
                    theme.value_hi
                } else {
                    color(band_max(top_row, lo, hi))
                };
                Span::styled("\u{2580}", Style::default().fg(top_color).bg(bot_color))
            })
            .collect();
        lines.push(Line::from(spans));
    }

    f.render_widget(Paragraph::new(lines), area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn band_max_reads_in_range() {
        let row = [-90.0, -50.0, -70.0, -60.0];
        assert_eq!(band_max(&row, 0, 4), -50.0, "max over the whole row");
        assert_eq!(band_max(&row, 2, 4), -60.0, "max over a sub-range");
    }

    #[test]
    fn band_max_clamps_out_of_range_indices() {
        let row = [-90.0, -50.0, -70.0];
        // End past the row length must clamp, not panic.
        assert_eq!(band_max(&row, 1, 99), -50.0);
        // A start at/after the row length yields the floor (empty span).
        assert_eq!(band_max(&row, 3, 9), f32::NEG_INFINITY);
        assert_eq!(band_max(&row, 5, 2), f32::NEG_INFINITY);
    }

    #[test]
    fn band_max_empty_row_is_floor() {
        assert_eq!(band_max(&[], 0, 4), f32::NEG_INFINITY);
    }

    #[test]
    fn unzoomed_columns_cover_every_bin_exactly_once() {
        let c = Columns::new(1024, 0, 1024, 128, BinAxis::FftBins);
        let (first, _) = c.range(0);
        let (_, last) = c.range(127);
        assert_eq!(first, 0, "the first column starts at the first bin");
        assert_eq!(last, 1024, "the last column ends at the last bin");
        // Contiguous: each column picks up where the previous one stopped.
        for col in 1..128 {
            assert_eq!(
                c.range(col).0,
                c.range(col - 1).1,
                "gap or overlap at column {col}"
            );
        }
    }

    #[test]
    fn zoom_keeps_the_centre_slice() {
        let c = Columns::new(1024, 384, 256, 128, BinAxis::FftBins);
        let (first, _) = c.range(0);
        let (_, last) = c.range(127);
        assert_eq!(first, 384, "a quarter of the way in");
        assert_eq!(last, 640, "and out again: 256 bins around the centre");
    }

    #[test]
    fn columns_follow_the_bin_window_at_uneven_zoom() {
        let axis = BinAxis::FftBins;
        let window = axis.window(100, 10.0, 10, 3).unwrap();
        let columns = Columns::new(10, window.first_bin, window.bin_count, 3, axis);
        assert_eq!(columns.range(0), (4, 5));
        assert_eq!(columns.range(1), (5, 6));
        assert_eq!(columns.range(2), (6, 7));
        for col in 0..3 {
            assert_eq!(
                axis.frequency_of_bin(window.left_hz, window.span_hz, window.bin_count, col),
                Some(99.0 + col as f64)
            );
        }
    }

    #[test]
    fn a_column_is_never_empty_however_odd_the_geometry() {
        // More columns than bins: every column still reads at least one bin,
        // rather than an empty span that would paint the whole plot at the floor.
        let c = Columns::new(8, 0, 8, 200, BinAxis::FftBins);
        for col in 0..200 {
            let (lo, hi) = c.range(col);
            assert!(hi > lo, "column {col} is empty");
            assert!(hi <= 8, "column {col} runs past the row");
        }
    }

    #[test]
    fn zoomed_columns_are_nothing_like_the_naive_full_range_mapping() {
        // The bug this pins. The focus readout used to map its column onto the
        // *whole* row - `col * row_bins / cols` - while the grid mapped it
        // through the zoom window. At zoom 1 the two agree, which is why it
        // went unnoticed; zoomed in, the readout quoted the level of a
        // completely different frequency from the column it pointed at.
        let (row_bins, cols) = (1024usize, 128usize);
        let naive = |col: usize| col * row_bins / cols;

        let unzoomed = Columns::new(row_bins, 0, row_bins, cols, BinAxis::FftBins);
        for col in 0..cols {
            assert_eq!(unzoomed.range(col).0, naive(col), "zoom 1 hides the bug");
        }

        let zoomed = Columns::new(row_bins, 384, 256, cols, BinAxis::FftBins);
        assert_eq!(zoomed.range(0).0, 384);
        assert_eq!(
            naive(0),
            0,
            "the naive mapping reads a bin that is off screen"
        );
        // They coincide only at the centre, the one place the old readout was right.
        assert_eq!(zoomed.range(cols / 2).0, naive(cols / 2));
    }

    #[test]
    fn degenerate_input_does_not_underflow() {
        let c = Columns::new(0, 0, 0, 40, BinAxis::FftBins);
        let (lo, hi) = c.range(0);
        assert!(hi > lo);
        let zero_cols = Columns::new(1024, 384, 256, 0, BinAxis::FftBins);
        let _ = zero_cols.range(0);
    }
}
