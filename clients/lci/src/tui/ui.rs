//! Pure rendering: given an [`App`], draw the current frame. No I/O, no state mutation. Design goal
//! is calm and uncluttered — a thin title bar, a single content table, semantic status colors, and a
//! one-line key hint.

use super::app::{App, ToastKind, View};
use crate::api::TaskRow;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState};
use ratatui::Frame;
use time::OffsetDateTime;

/// Muted chrome color for borders/labels.
const CHROME: Color = Color::DarkGray;

/// Draw one frame.
pub fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // title / tabs
            Constraint::Min(3),    // content
            Constraint::Length(1), // status bar
            Constraint::Length(1), // key hint / toast
        ])
        .split(f.area());

    draw_title(f, chunks[0], app);
    match app.view {
        View::Repositories => draw_repositories(f, chunks[1], app),
        View::Runs => draw_runs(f, chunks[1], app),
    }
    draw_status(f, chunks[2], app);
    draw_footer(f, chunks[3], app);

    if let Some(confirm) = &app.confirm {
        draw_confirm(f, &confirm.prompt);
    }
    if app.show_help {
        draw_help(f);
    }
}

/// Title bar with the two tabs and a subtle app label.
fn draw_title(f: &mut Frame, area: Rect, app: &App) {
    let tab = |name: &str, active: bool| {
        if active {
            Span::styled(
                format!(" {name} "),
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(format!(" {name} "), Style::default().fg(Color::Gray))
        }
    };
    let line = Line::from(vec![
        Span::styled(
            " lci ",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("· ", Style::default().fg(CHROME)),
        tab("1 Repositories", app.view == View::Repositories),
        Span::raw(" "),
        tab("2 Runs", app.view == View::Runs),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

/// Repositories table.
fn draw_repositories(f: &mut Frame, area: Rect, app: &App) {
    let header = Row::new(vec![
        Cell::from("REPOSITORY"),
        Cell::from("STATUS"),
        Cell::from("TASKS"),
        Cell::from("LAST TASK"),
        Cell::from("APPROVED BY"),
    ])
    .style(Style::default().fg(CHROME).add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = app
        .repos
        .iter()
        .map(|r| {
            Row::new(vec![
                Cell::from(format!("{}/{}", r.owner, r.name)),
                Cell::from(Span::styled(r.status.clone(), status_style(&r.status))),
                Cell::from(r.task_count.to_string()),
                Cell::from(fmt_ts(r.last_task_at)),
                Cell::from(r.approved_by.clone().unwrap_or_else(|| "—".into())),
            ])
        })
        .collect();

    let title = format!(
        " Repositories · filter: {} ({} shown) ",
        app.repo_filter.label(),
        app.repos.len()
    );
    let widths = [
        Constraint::Percentage(40),
        Constraint::Length(10),
        Constraint::Length(7),
        Constraint::Length(20),
        Constraint::Percentage(20),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(bordered(&title))
        .row_highlight_style(selected_style())
        .highlight_symbol("▏");

    let mut state = TableState::default();
    if !app.repos.is_empty() {
        state.select(Some(app.repo_selected));
    }
    render_table_or_empty(
        f,
        area,
        table,
        &mut state,
        app.repos.is_empty(),
        &title,
        empty_repos_hint(app),
    );
}

/// Runs table.
fn draw_runs(f: &mut Frame, area: Rect, app: &App) {
    let header = Row::new(vec![
        Cell::from("STATUS"),
        Cell::from("REPOSITORY"),
        Cell::from("TARGET"),
        Cell::from("KIND"),
        Cell::from("AGE"),
        Cell::from("JOB"),
    ])
    .style(Style::default().fg(CHROME).add_modifier(Modifier::BOLD));

    let visible = app.visible_tasks();
    let rows: Vec<Row> = visible
        .iter()
        .map(|t| {
            Row::new(vec![
                Cell::from(Span::styled(t.status.clone(), status_style(&t.status))),
                Cell::from(repo_label(t)),
                Cell::from(target_label(t)),
                Cell::from(t.kind.clone()),
                Cell::from(age(t.created_at)),
                Cell::from(t.job_name.clone().unwrap_or_else(|| "—".into())),
            ])
        })
        .collect();

    let filter = if app.runs_active_only {
        "active"
    } else {
        "all"
    };
    let title = format!(" Runs · filter: {} ({} shown) ", filter, visible.len());
    let widths = [
        Constraint::Length(17),
        Constraint::Percentage(30),
        Constraint::Length(14),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Percentage(20),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(bordered(&title))
        .row_highlight_style(selected_style())
        .highlight_symbol("▏");

    let mut state = TableState::default();
    if !visible.is_empty() {
        state.select(Some(app.run_selected));
    }
    render_table_or_empty(
        f,
        area,
        table,
        &mut state,
        visible.is_empty(),
        &title,
        "No runs match the current filter. Press f to show all, r to refresh.".into(),
    );
}

/// Render the table, or an inline empty-status line inside the same bordered block if there are no
/// rows (empty states are status lines, not centered placards).
#[allow(clippy::too_many_arguments)]
fn render_table_or_empty(
    f: &mut Frame,
    area: Rect,
    table: Table,
    state: &mut TableState,
    is_empty: bool,
    title: &str,
    hint: String,
) {
    if is_empty {
        let para = Paragraph::new(Line::from(Span::styled(
            hint,
            Style::default().fg(Color::Gray),
        )))
        .block(bordered(title));
        f.render_widget(para, area);
    } else {
        f.render_stateful_widget(table, area, state);
    }
}

/// Status bar: identity, permissions summary, token countdown, host, re-auth flag.
fn draw_status(f: &mut Frame, area: Rect, app: &App) {
    let identity = app.me.as_ref().map(|m| m.identity()).unwrap_or("unknown");
    let perms = app
        .me
        .as_ref()
        .map(|m| perm_summary(&m.permissions))
        .unwrap_or_default();

    let mut spans = vec![
        Span::styled(
            format!(" {identity} "),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(perms, Style::default().fg(Color::Gray)),
        Span::styled("  ·  ", Style::default().fg(CHROME)),
        Span::styled(
            format!("host {}", app.api_host),
            Style::default().fg(Color::Gray),
        ),
        Span::styled("  ·  ", Style::default().fg(CHROME)),
        token_span(app),
    ];
    if app.reauth_needed {
        spans.push(Span::styled("  ·  ", Style::default().fg(CHROME)));
        spans.push(Span::styled(
            "⚠ re-auth needed (lci login)",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The token expiry countdown span, colored by urgency.
fn token_span(app: &App) -> Span<'static> {
    let Some(exp) = app.token_expires_at else {
        return Span::styled("token …", Style::default().fg(Color::Gray));
    };
    let now = crate::auth::now_unix();
    let remaining = exp - now;
    let (text, color) = if remaining <= 0 {
        ("token expired".to_string(), Color::Red)
    } else {
        let mins = remaining / 60;
        let secs = remaining % 60;
        let color = if remaining < 60 {
            Color::Yellow
        } else {
            Color::Gray
        };
        (format!("token {mins}m{secs:02}s"), color)
    };
    Span::styled(text, Style::default().fg(color))
}

/// Footer: a transient toast if present, else the contextual key hint.
fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    if let Some(toast) = &app.toast {
        let color = match toast.kind {
            ToastKind::Info => Color::Cyan,
            ToastKind::Success => Color::Green,
            ToastKind::Error => Color::Red,
        };
        let line = Line::from(Span::styled(
            format!(" {}", toast.text),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
        f.render_widget(Paragraph::new(line), area);
        return;
    }
    let hint = match app.view {
        View::Repositories => {
            " q quit · Tab/1/2 view · j/k move · a approve · d deny · f filter · r refresh · ? help"
        }
        View::Runs => {
            " q quit · Tab/1/2 view · j/k move · c cancel · f active/all · r refresh · ? help"
        }
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(hint, Style::default().fg(CHROME)))),
        area,
    );
}

/// A centered confirmation modal.
fn draw_confirm(f: &mut Frame, prompt: &str) {
    let area = centered_rect(60, 20, f.area());
    f.render_widget(Clear, area);
    let text = vec![
        Line::from(Span::styled(
            prompt.to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Enter / y to confirm · Esc / n to cancel",
            Style::default().fg(Color::Gray),
        )),
    ];
    let para = Paragraph::new(text).alignment(Alignment::Center).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow))
            .title(" Confirm "),
    );
    f.render_widget(para, area);
}

/// The help overlay.
fn draw_help(f: &mut Frame) {
    let area = centered_rect(60, 60, f.area());
    f.render_widget(Clear, area);
    let lines = vec![
        Line::from(Span::styled(
            "lci — keybindings",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("  q / Esc      quit"),
        Line::from("  Tab / 1 / 2  switch view"),
        Line::from("  ↑/↓ or j/k   move selection"),
        Line::from("  r            refresh now"),
        Line::from("  f            cycle filter (repos) / active-all (runs)"),
        Line::from("  a            approve selected repository"),
        Line::from("  d            deny selected repository"),
        Line::from("  c            cancel selected run"),
        Line::from("  ?            toggle this help"),
        Line::from(""),
        Line::from(Span::styled(
            "  Actions gated by your token permissions.",
            Style::default().fg(Color::Gray),
        )),
    ];
    let para = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(CHROME))
            .title(" Help "),
    );
    f.render_widget(para, area);
}

// --- small helpers ---

fn bordered(title: &str) -> Block<'_> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(CHROME))
        .title(title.to_string())
}

fn selected_style() -> Style {
    Style::default()
        .bg(Color::Rgb(40, 44, 52))
        .add_modifier(Modifier::BOLD)
}

/// Semantic color for a repo/task status word.
fn status_style(status: &str) -> Style {
    let color = match status {
        "pending" | "waiting_for_index" | "queued" | "received" => Color::Yellow,
        "approved" | "succeeded" => Color::Green,
        "disabled" | "failed" | "timed_out" | "cancelled" => Color::Red,
        "running" | "posting_result" => Color::Cyan,
        _ => Color::Gray,
    };
    Style::default().fg(color)
}

fn empty_repos_hint(app: &App) -> String {
    format!(
        "No {} repositories. Press f to change the filter, r to refresh.",
        app.repo_filter.label()
    )
}

/// A compact permissions summary for the status bar (count + the action verbs we care about).
fn perm_summary(perms: &[String]) -> String {
    let mut verbs = Vec::new();
    for cap in ["repo:approve", "repo:deny", "task:cancel"] {
        if perms.iter().any(|p| p == cap) {
            verbs.push(cap.rsplit(':').next().unwrap_or(cap));
        }
    }
    if verbs.is_empty() {
        format!("{} perms (read-only)", perms.len())
    } else {
        format!("can: {}", verbs.join("/"))
    }
}

fn repo_label(t: &TaskRow) -> String {
    match (&t.repo_owner, &t.repo_name) {
        (Some(o), Some(n)) => format!("{o}/{n}"),
        _ => format!("repo#{}", t.repository_id),
    }
}

/// A `#PR`/`#issue` style target label.
fn target_label(t: &TaskRow) -> String {
    let sigil = match t.target_type.as_str() {
        "pull_request" => "PR",
        "issue" => "issue",
        "" => "target",
        other => other,
    };
    format!("{sigil} #{}", t.target_id)
}

/// A human age like `3m`, `2h`, `5d` from a creation timestamp.
fn age(created: OffsetDateTime) -> String {
    let secs = (crate::auth::now_unix() - created.unix_timestamp()).max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// Format an optional rfc3339 timestamp as a compact `YYYY-MM-DD HH:MM`, or `—`.
fn fmt_ts(ts: Option<OffsetDateTime>) -> String {
    match ts {
        Some(t) => format!(
            "{:04}-{:02}-{:02} {:02}:{:02}",
            t.year(),
            t.month() as u8,
            t.day(),
            t.hour(),
            t.minute()
        ),
        None => "—".into(),
    }
}

/// A rect centered in `r` at the given percentage size.
fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(status: &str, target_type: &str, target_id: i64) -> TaskRow {
        TaskRow {
            id: uuid::Uuid::nil(),
            repository_id: 3,
            target_type: target_type.into(),
            target_id,
            command_text: "review".into(),
            kind: "review".into(),
            status: status.into(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            started_at: None,
            completed_at: None,
            repo_owner: Some("vymalo".into()),
            repo_name: Some("lci".into()),
            job_name: None,
            error_detail: None,
        }
    }

    #[test]
    fn target_labels_are_readable() {
        assert_eq!(target_label(&task("running", "pull_request", 12)), "PR #12");
        assert_eq!(target_label(&task("running", "issue", 7)), "issue #7");
    }

    #[test]
    fn repo_label_falls_back_to_id() {
        let mut t = task("running", "pull_request", 1);
        t.repo_owner = None;
        assert_eq!(repo_label(&t), "repo#3");
    }

    #[test]
    fn status_colors_are_semantic() {
        assert_eq!(status_style("approved").fg, Some(Color::Green));
        assert_eq!(status_style("pending").fg, Some(Color::Yellow));
        assert_eq!(status_style("failed").fg, Some(Color::Red));
        assert_eq!(status_style("running").fg, Some(Color::Cyan));
    }

    #[test]
    fn perm_summary_lists_capabilities() {
        let s = perm_summary(&["repo:approve".into(), "repo:deny".into()]);
        assert!(s.contains("approve"));
        assert!(s.contains("deny"));
        let ro = perm_summary(&["repo:read".into()]);
        assert!(ro.contains("read-only"));
    }

    #[test]
    fn fmt_ts_handles_none() {
        assert_eq!(fmt_ts(None), "—");
        assert!(fmt_ts(Some(OffsetDateTime::UNIX_EPOCH)).starts_with("1970-01-01"));
    }
}
