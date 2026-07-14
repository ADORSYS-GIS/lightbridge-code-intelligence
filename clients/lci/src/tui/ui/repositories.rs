//! The Repositories list view: owner/name, status, task count, last-task age, and who approved it.

use super::super::app::App;
use super::helpers::{empty_repos_hint, fmt_ts, render_table_or_empty, status_chip, title_line};
use crate::theme::{Theme, status_label};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Cell, Row, Table};

/// Repositories table.
pub(super) fn draw_repositories(f: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let header = Row::new(vec![
        Cell::from("REPOSITORY"),
        Cell::from("STATUS"),
        Cell::from(Line::from("TASKS").alignment(Alignment::Right)),
        Cell::from(Line::from("LAST TASK").alignment(Alignment::Right)),
        Cell::from("APPROVED BY"),
    ])
    .style(theme.header_style());

    let rows: Vec<Row> = app
        .repos
        .iter()
        .map(|r| {
            Row::new(vec![
                Cell::from(Span::styled(
                    format!("{}/{}", r.owner, r.name),
                    theme.text(),
                )),
                Cell::from(Span::styled(
                    status_label(&r.status).to_string(),
                    Style::default()
                        .fg(theme.status_color(&r.status))
                        .add_modifier(Modifier::BOLD),
                )),
                Cell::from(Line::from(r.task_count.to_string()).alignment(Alignment::Right)),
                Cell::from(
                    Line::from(Span::styled(fmt_ts(r.last_task_at), theme.muted_text()))
                        .alignment(Alignment::Right),
                ),
                Cell::from(Span::styled(
                    r.approved_by.clone().unwrap_or_else(|| "—".into()),
                    theme.muted_text(),
                )),
            ])
        })
        .collect();

    let widths = [
        Constraint::Percentage(40),
        Constraint::Length(10),
        Constraint::Length(7),
        Constraint::Length(18),
        Constraint::Percentage(20),
    ];
    let filter_chip = status_chip(app.repo_filter.label(), theme);
    let title = title_line("Repositories", app.repos.len(), Some(filter_chip), theme);
    let table = Table::new(rows, widths)
        .header(header)
        .row_highlight_style(theme.selected_row_style())
        .highlight_symbol("▌");

    render_table_or_empty(
        f,
        area,
        table,
        app.repo_selected,
        app.repos.is_empty(),
        title,
        empty_repos_hint(app),
        theme,
    );
}
