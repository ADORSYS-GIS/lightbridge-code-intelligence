//! The Runs list view: status, repository, target (PR/issue), kind, age, and job name.

use super::super::app::App;
use super::helpers::{
    age, render_table_or_empty, repo_label, status_chip, target_label, title_line,
};
use crate::theme::{Theme, status_label};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Row, Table};

/// Runs table.
pub(super) fn draw_runs(f: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let header = Row::new(vec![
        Cell::from("STATUS"),
        Cell::from("REPOSITORY"),
        Cell::from("TARGET"),
        Cell::from("KIND"),
        Cell::from(Line::from("AGE").alignment(Alignment::Right)),
        Cell::from("JOB"),
    ])
    .style(theme.header_style());

    let visible = app.visible_tasks();
    let rows: Vec<Row> = visible
        .iter()
        .map(|t| {
            Row::new(vec![
                Cell::from(Span::styled(
                    status_label(&t.status).to_string(),
                    Style::default()
                        .fg(theme.status_color(&t.status))
                        .add_modifier(Modifier::BOLD),
                )),
                Cell::from(Span::styled(repo_label(t), theme.text())),
                Cell::from(Span::styled(target_label(t), theme.text())),
                Cell::from(Span::styled(t.kind.clone(), theme.muted_text())),
                Cell::from(
                    Line::from(Span::styled(age(t.created_at), theme.muted_text()))
                        .alignment(Alignment::Right),
                ),
                Cell::from(Span::styled(
                    t.job_name.clone().unwrap_or_else(|| "—".into()),
                    theme.muted_text(),
                )),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(16),
        Constraint::Percentage(30),
        Constraint::Length(12),
        Constraint::Length(8),
        Constraint::Length(6),
        Constraint::Percentage(20),
    ];
    let filter = if app.runs_active_only {
        "active"
    } else {
        "all"
    };
    let filter_chip = status_chip(filter, theme);
    let title = title_line("Runs", visible.len(), Some(filter_chip), theme);
    let table = Table::new(rows, widths)
        .header(header)
        .row_highlight_style(theme.selected_row_style())
        .highlight_symbol("▌");

    render_table_or_empty(
        f,
        area,
        table,
        app.run_selected,
        visible.is_empty(),
        title,
        "no runs match the current filter — press f to show all, r to refresh".into(),
        theme,
    );
}
