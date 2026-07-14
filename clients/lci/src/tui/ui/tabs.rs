//! The pill-tab bar under the header: active tab gets an accent background, inactive tabs muted on
//! the surface. The Run Detail page shows as a `▸`-joined sub-page of the Runs tab.

use super::super::app::{App, View};
use crate::theme::Theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

/// The pill-tab bar: active tab gets an accent background, inactive tabs muted on the surface.
pub(super) fn draw_tabs(f: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    f.render_widget(Block::default().style(theme.surface_style()), area);

    let pill = |label: String, active: bool| -> Vec<Span<'static>> {
        if active {
            vec![Span::styled(
                format!(" {label} "),
                Style::default()
                    .fg(theme.on_accent())
                    .bg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )]
        } else {
            vec![Span::styled(
                format!(" {label} "),
                Style::default().fg(theme.muted).bg(theme.surface),
            )]
        }
    };

    let mut spans = vec![Span::raw(" ")];
    spans.extend(pill(
        format!("Repositories ({})", app.repos.len()),
        app.view == View::Repositories,
    ));
    spans.push(Span::styled(" ", theme.surface_style()));
    spans.extend(pill(
        format!("Runs ({})", app.tasks.len()),
        // The detail page is a sub-page of Runs — keep the Runs pill active while it's open.
        matches!(app.view, View::Runs | View::Detail),
    ));
    if app.view == View::Detail {
        spans.push(Span::styled(" ▸ ", theme.muted_text()));
        spans.extend(pill("Run Detail".to_string(), true));
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)).style(theme.surface_style()),
        area,
    );
}
