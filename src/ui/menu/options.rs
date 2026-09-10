// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MusiThang <viktor.laszlo92@protonmail.com>

//! The menu's right column: settings exposed by the active device.

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::{
    state::{DeviceOptionUpdate, InputMode, SdrMetrics},
    ui::chrome,
};

/// The heading. Says what the pane is for, in the future tense, because that is
/// the only true tense for it right now.
const HEADING: &str = "Settings will live here.";

/// The joke, and also the truth. Written without the `// ` so it can wrap like
/// any other prose: a comment that runs off the edge of a narrow pane is still a
/// comment the reader only sees half of.
const TODO: &[&str] = &[
    "TODO: settings go here",
    "left blank on purpose, not by accident",
];

/// The pane as lines, for an inner width of `iw`.
///
/// Separate from drawing for the same reason `keys::lines` is: the wrapping is
/// the only thing here that can be wrong, and it can be checked without a
/// terminal.
fn empty_lines(iw: usize, theme: &crate::Theme) -> Vec<Line<'static>> {
    let mut out = vec![Line::from("")];
    for row in chrome::wrap(HEADING, iw.saturating_sub(4), 3) {
        out.push(Line::from(Span::styled(
            format!("  {row}"),
            Style::default()
                .fg(theme.value_hi)
                .add_modifier(Modifier::BOLD),
        )));
    }
    out.push(Line::from(""));
    for comment in TODO {
        // Every wrapped row keeps the `// `, which is how a wrapped comment
        // looks in the source it is pretending to be.
        for row in chrome::wrap(comment, iw.saturating_sub(5), 3) {
            out.push(Line::from(Span::styled(
                format!("  // {row}"),
                Style::default().fg(theme.border_dim),
            )));
        }
    }
    out
}

fn lines(
    m: &SdrMetrics,
    selected: usize,
    iw: usize,
    height: usize,
    theme: &crate::Theme,
) -> Vec<Line<'static>> {
    if let InputMode::DeviceOptionInput { id, error } = &m.ui.input_mode {
        return editor_lines(m, id, error.as_deref(), iw, theme);
    }
    if m.device_options.is_empty() {
        return empty_lines(iw, theme);
    }

    let selected = selected.min(m.device_options.len() - 1);
    let mut out = option_header(iw, height, theme);
    let visible = height.saturating_sub(out.len());
    let first = scroll_offset(selected, m.device_options.len(), visible);
    for (index, option) in m
        .device_options
        .iter()
        .enumerate()
        .skip(first)
        .take(visible)
    {
        out.push(option_line(
            option,
            index == selected,
            &m.ui.device_option_update,
            iw,
            theme,
        ));
    }
    out
}

fn editor_lines(
    m: &SdrMetrics,
    id: &str,
    error: Option<&str>,
    iw: usize,
    theme: &crate::Theme,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    if let Some(option) = m.device_options.iter().find(|option| option.id == id) {
        out.push(Line::from(Span::styled(
            fit_text(&format!("{}: {}", option.label, option.selected_choice), iw),
            Style::default().fg(theme.label),
        )));
        out.push(Line::from(Span::styled(
            fit_text(&format!("Value: {}_", m.ui.input_buf), iw),
            Style::default().fg(theme.value_hi),
        )));
        if let Some(range) = &option.integer_range {
            for row in chrome::wrap(
                &format!("Integer {} to {}", range.start(), range.end()),
                iw,
                3,
            ) {
                out.push(Line::from(Span::styled(
                    row,
                    Style::default().fg(theme.label),
                )));
            }
        }
    }
    let error = error.or_else(|| {
        (!m.device_options.iter().any(|option| option.id == id))
            .then_some("Option is no longer available. Esc cancels.")
    });
    if let Some(error) = error {
        for row in chrome::wrap(error, iw, 4) {
            out.push(Line::from(Span::styled(
                row,
                Style::default().fg(theme.status_warn),
            )));
        }
    }
    out
}

fn option_header(iw: usize, height: usize, theme: &crate::Theme) -> Vec<Line<'static>> {
    let heading = Line::from(Span::styled(
        fit_text("  Device options", iw),
        Style::default()
            .fg(theme.value_hi)
            .add_modifier(Modifier::BOLD),
    ));
    match height {
        0 | 1 => Vec::new(),
        2 => vec![heading],
        3 => vec![heading, Line::from("")],
        _ => vec![Line::from(""), heading, Line::from("")],
    }
}

fn option_line(
    option: &crate::hardware::DeviceOption,
    active: bool,
    update: &DeviceOptionUpdate,
    iw: usize,
    theme: &crate::Theme,
) -> Line<'static> {
    let marker = if active { "\u{25b8} " } else { "  " };
    let value_style = Style::default()
        .fg(if active { theme.value_hi } else { theme.value })
        .add_modifier(if active {
            Modifier::BOLD
        } else {
            Modifier::empty()
        });
    if iw < 7 {
        return Line::from(Span::styled(
            fit_text(marker, iw),
            Style::default().fg(theme.border_accent),
        ));
    }
    let content_width = iw - 7;
    let label_width = content_width.min(18).min(content_width / 2);
    let choice_width = content_width - label_width;
    let label = fit_cell(&option.label, label_width);
    let shown = match update {
        DeviceOptionUpdate::Pending { request, .. } if request.id == option.id => {
            format!("{} -> {}...", option.selected_choice, request.choice)
        }
        DeviceOptionUpdate::Failed { id } if id == &option.id => {
            format!("{} (failed)", option.selected_choice)
        }
        _ => option.selected_choice.clone(),
    };
    let choice = fit_text(&shown, choice_width);
    Line::from(vec![
        Span::styled(marker, Style::default().fg(theme.border_accent)),
        Span::styled(label, Style::default().fg(theme.label)),
        Span::raw(" "),
        Span::styled(format!("\u{25c0} {choice} \u{25b6}"), value_style),
    ])
}

fn scroll_offset(cursor: usize, total: usize, visible: usize) -> usize {
    if visible == 0 || total <= visible {
        return 0;
    }
    cursor
        .saturating_sub(visible - 1)
        .min(total.saturating_sub(visible))
}

fn fit_cell(text: &str, width: usize) -> String {
    let mut fitted = fit_text(text, width);
    fitted.push_str(&" ".repeat(width.saturating_sub(text_width(&fitted))));
    fitted
}

fn fit_text(text: &str, width: usize) -> String {
    let mut fitted = String::new();
    for character in text.chars() {
        fitted.push(character);
        if text_width(&fitted) > width {
            fitted.pop();
            break;
        }
    }
    fitted
}

fn text_width(text: &str) -> usize {
    Line::from(text).width()
}

pub fn render(f: &mut Frame, area: Rect, m: &SdrMetrics, selected: usize, theme: &crate::Theme) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let all = lines(
        m,
        selected,
        area.width as usize,
        area.height as usize,
        theme,
    );
    let shown: Vec<Line> = all.into_iter().take(area.height as usize).collect();
    f.render_widget(Paragraph::new(shown), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// The pane's whole job right now is to name itself and admit it is empty,
    /// so both halves are worth pinning.
    #[test]
    fn the_empty_state_names_itself_and_admits_it() {
        let text: String = lines(&SdrMetrics::fixture(), 0, 60, 20, &crate::Theme::sdr())
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Settings will live here"), "{text}");
        assert!(text.contains("TODO"), "{text}");
    }

    /// The copy wraps, so it has to keep fitting the narrow single column form
    /// as well as a wide one. A row wider than the pane is a row the reader only
    /// sees half of.
    #[test]
    fn every_row_fits_the_pane() {
        for iw in [28, 40, 44, 60, 100] {
            for line in empty_lines(iw, &crate::Theme::sdr()) {
                assert!(
                    line.width() <= iw,
                    "a {}-wide row does not fit {iw} columns: {line:?}",
                    line.width()
                );
            }
        }
    }

    /// House style.
    #[test]
    fn the_copy_uses_no_em_dashes() {
        assert!(!HEADING.contains('\u{2014}'));
        assert!(TODO.iter().all(|t| !t.contains('\u{2014}')));
    }

    #[test]
    fn options_name_their_current_choices() {
        let mut m = SdrMetrics::fixture();
        Arc::make_mut(&mut m.device_options).push(crate::hardware::DeviceOption {
            id: "bandwidth".into(),
            label: "Bandwidth".into(),
            choices: vec!["Narrow".into(), "Wide".into()],
            selected_choice: "Wide".into(),
            integer_range: None,
        });
        let text = lines(&m, 0, 60, 20, &crate::Theme::sdr())
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Bandwidth"), "{text}");
        assert!(text.contains("Wide"), "{text}");
    }

    #[test]
    fn the_selected_option_stays_in_a_short_viewport() {
        let mut m = SdrMetrics::fixture();
        for index in 0..6 {
            Arc::make_mut(&mut m.device_options).push(crate::hardware::DeviceOption {
                id: format!("option-{index}"),
                label: format!("Option {index}"),
                choices: vec!["Off".into(), "On".into()],
                selected_choice: "Off".into(),
                integer_range: None,
            });
        }

        let text = lines(&m, 5, 60, 5, &crate::Theme::sdr())
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Option 5"), "{text}");
        assert!(!text.contains("Option 0"), "{text}");
    }

    #[test]
    fn the_selected_option_uses_the_only_available_row() {
        let mut m = SdrMetrics::fixture();
        Arc::make_mut(&mut m.device_options).push(crate::hardware::DeviceOption {
            id: "bandwidth".into(),
            label: "Bandwidth".into(),
            choices: vec!["Narrow".into(), "Wide".into()],
            selected_choice: "Wide".into(),
            integer_range: None,
        });

        let text = lines(&m, 0, 60, 1, &crate::Theme::sdr())
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Bandwidth"), "{text}");
        assert!(text.contains("Wide"), "{text}");
    }

    #[test]
    fn backend_text_never_exceeds_the_pane_width() {
        let mut m = SdrMetrics::fixture();
        Arc::make_mut(&mut m.device_options).push(crate::hardware::DeviceOption {
            id: "long".into(),
            label: "A device-provided label that is much too long".into(),
            choices: vec!["A device-provided choice that is much too long".into()],
            selected_choice: "A device-provided choice that is much too long".into(),
            integer_range: None,
        });

        for width in [1, 4, 7, 8, 12, 20, 28] {
            for line in lines(&m, 0, width, 6, &crate::Theme::sdr()) {
                assert!(
                    line.width() <= width,
                    "a {}-wide row does not fit {width} columns: {line:?}",
                    line.width()
                );
            }
        }
    }

    #[test]
    fn pending_change_keeps_the_accepted_choice_visible() {
        let mut m = SdrMetrics::fixture();
        Arc::make_mut(&mut m.device_options).push(crate::hardware::DeviceOption {
            id: "bandwidth".into(),
            label: "Bandwidth".into(),
            choices: vec!["Narrow".into(), "Wide".into()],
            selected_choice: "Narrow".into(),
            integer_range: None,
        });
        m.ui.device_option_update = DeviceOptionUpdate::Pending {
            request: crate::event::DeviceOptionRequest {
                id: "bandwidth".into(),
                label: "Bandwidth".into(),
                choice: "Wide".into(),
            },
            quit_requested: false,
        };

        let text = lines(&m, 0, 80, 1, &crate::Theme::sdr())
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Narrow -> Wide..."), "{text}");
    }

    #[test]
    fn failed_change_keeps_the_accepted_choice_visible() {
        let mut m = SdrMetrics::fixture();
        Arc::make_mut(&mut m.device_options).push(crate::hardware::DeviceOption {
            id: "bandwidth".into(),
            label: "Bandwidth".into(),
            choices: vec!["Narrow".into(), "Wide".into()],
            selected_choice: "Narrow".into(),
            integer_range: None,
        });
        m.ui.device_option_update = DeviceOptionUpdate::Failed {
            id: "bandwidth".into(),
        };

        let text = lines(&m, 0, 80, 1, &crate::Theme::sdr())
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Narrow (failed)"), "{text}");
        assert!(!text.contains("Wide"), "{text}");
    }

    #[test]
    fn numeric_editor_shows_validation_errors_and_fits_narrow_panes() {
        let mut m = SdrMetrics::fixture();
        m.device_options = Arc::new(vec![crate::hardware::DeviceOption {
            id: "gain".into(),
            label: "Gain".into(),
            choices: vec!["0".into(), "12".into()],
            selected_choice: "0".into(),
            integer_range: Some(-100..=100),
        }]);
        m.ui.input_mode = InputMode::DeviceOptionInput {
            id: "gain".into(),
            error: Some("Enter an advertised integer from -100 to 100".into()),
        };
        m.ui.input_buf = "101".into();
        for width in [0, 1, 4, 7, 12, 28, 60] {
            for line in lines(&m, 0, width, 20, &crate::Theme::sdr()) {
                assert!(line.width() <= width, "{line:?}");
            }
        }
        let text = lines(&m, 0, 60, 20, &crate::Theme::sdr())
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Gain: 0"), "{text}");
        assert!(text.contains("Value: 101_"), "{text}");
        assert!(text.contains("Enter an advertised integer"), "{text}");

        m.device_options = Arc::new(Vec::new());
        m.ui.input_mode = InputMode::DeviceOptionInput {
            id: "gain".into(),
            error: None,
        };
        let text = lines(&m, 0, 60, 20, &crate::Theme::sdr())
            .into_iter()
            .map(|line| line.to_string())
            .collect::<String>();
        assert!(
            text.contains("Option is no longer available. Esc cancels."),
            "{text}"
        );
    }
}
