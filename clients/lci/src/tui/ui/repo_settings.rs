//! The Repo Settings page (story #500): a single bordered panel showing the repo's currently-
//! configured review preset and a free-text input to change it.

use super::super::app::App;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};

use crate::theme::Theme;

pub(super) fn draw_repo_settings(f: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let Some(s) = app.repo_settings.as_ref() else {
        return;
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.border))
        .style(Style::default().bg(theme.background))
        .title(Line::from(Span::styled(
            format!("▐ Review preset — {} ▌", s.repo_label),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let current_line = if !s.loaded {
        Line::from(Span::styled("loading current preset…", theme.muted_text()))
    } else {
        match s.current.as_ref().and_then(|c| c.preset.as_deref()) {
            Some(preset) => Line::from(vec![
                Span::styled("current: ", theme.muted_text()),
                Span::styled(preset.to_string(), theme.text()),
            ]),
            None => Line::from(Span::styled(
                "current: (none declared — platform default applies)",
                theme.muted_text(),
            )),
        }
    };

    let input_label = if s.saving { "saving: " } else { "new:     " };
    let input_line = Line::from(vec![
        Span::styled(input_label, theme.muted_text()),
        Span::styled(s.input.clone(), theme.text()),
        Span::styled("▏", Style::default().fg(theme.accent)), // cursor
    ]);

    let hint_line = Line::from(Span::styled(
        if app.can_configure_preset() {
            "type a preset name, Enter to commit it to .lightbridge-code-review.jsonc, Esc to cancel"
        } else {
            "you lack repo:configure — read-only"
        },
        theme.muted_text(),
    ));

    let paragraph = Paragraph::new(vec![
        current_line,
        Line::default(),
        input_line,
        Line::default(),
        hint_line,
    ]);
    f.render_widget(paragraph, inner);
}
