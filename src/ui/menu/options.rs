// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 MusiThang <viktor.laszlo92@protonmail.com>

use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::state::SdrMetrics;

fn lines(m: &SdrMetrics, selected: usize, theme: &crate::Theme) -> Vec<Line<'static>> {
    if m.device_options.is_empty() {
        return vec![
            Line::from(""),
            Line::from(Span::styled(
                "  This device has no configurable options.",
                Style::default().fg(theme.label),
            )),
        ];
    }

    let mut out = vec![
        Line::from(""),
        Line::from(Span::styled(
            "  Device options",
            Style::default()
                .fg(theme.value_hi)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    for (index, option) in m.device_options.iter().enumerate() {
        let active = index == selected;
        let marker = if active { "\u{25b8} " } else { "  " };
        let value = option.selected_value().unwrap_or("\u{2014}");
        out.push(Line::from(vec![
            Span::styled(
                marker,
                Style::default().fg(if active {
                    theme.border_accent
                } else {
                    theme.border_dim
                }),
            ),
            Span::styled(
                format!("{:<18}", option.label),
                Style::default().fg(theme.label),
            ),
            Span::styled(
                format!("\u{25c0} {value} \u{25b6}"),
                Style::default()
                    .fg(if active { theme.value_hi } else { theme.value })
                    .add_modifier(if active {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
        ]));
    }
    out
}

pub fn render(f: &mut Frame, area: Rect, m: &SdrMetrics, selected: usize, theme: &crate::Theme) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let shown: Vec<Line> = lines(m, selected, theme)
        .into_iter()
        .take(area.height as usize)
        .collect();
    f.render_widget(Paragraph::new(shown), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hardware::DeviceOption;

    #[test]
    fn an_option_names_its_current_value() {
        let mut m = SdrMetrics::fixture();
        m.device_options.push(DeviceOption {
            id: "rbw".into(),
            label: "RBW".into(),
            values: vec!["auto".into(), "10 kHz".into()],
            selected: 1,
        });
        let text = lines(&m, 0, &crate::Theme::sdr())
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("RBW"), "{text}");
        assert!(text.contains("10 kHz"), "{text}");
    }
}
