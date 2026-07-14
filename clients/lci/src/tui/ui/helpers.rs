//! Small formatting + layout utilities shared across the view modules: rect padding, width-correct
//! truncation, the k9s-style title/chip spans, the bordered-table-or-empty-state helper, and the
//! task-row label formatters used by both the Runs table and the Run Detail page.

use super::super::app::App;
use crate::api::TaskRow;
use crate::theme::Theme;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Table, TableState};
use time::OffsetDateTime;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// A k9s-style content title: `▐ Name ▌` + a count badge + an optional filter chip.
pub(super) fn title_line(
    name: &str,
    count: usize,
    chip: Option<Vec<Span<'static>>>,
    theme: &Theme,
) -> Line<'static> {
    let mut spans = vec![
        Span::styled(
            format!("▐ {name} ▌"),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {count} "),
            Style::default().fg(theme.on_accent()).bg(theme.secondary),
        ),
    ];
    if let Some(chip) = chip {
        spans.push(Span::raw(" "));
        spans.extend(chip);
    }
    Line::from(spans)
}

/// A `[label]` filter chip in warning color (matches the k9s "active filter" affordance).
pub(super) fn status_chip(label: &str, theme: &Theme) -> Vec<Span<'static>> {
    vec![Span::styled(
        format!("[{label}]"),
        Style::default()
            .fg(theme.warning)
            .add_modifier(Modifier::BOLD),
    )]
}

/// A bordered content block carrying a rich title line.
pub(super) fn bordered(title: Line<'static>, theme: &Theme) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.border))
        .style(Style::default().bg(theme.background))
        .title(title)
}

/// Render the table, or an inline muted status line inside the same bordered block when there are no
/// rows (empty states are inline status lines, not centered placards).
#[allow(clippy::too_many_arguments)]
pub(super) fn render_table_or_empty(
    f: &mut ratatui::Frame,
    area: Rect,
    table: Table,
    selected: usize,
    is_empty: bool,
    title: Line<'static>,
    hint: String,
    theme: &Theme,
) {
    let block = bordered(title, theme);
    if is_empty {
        let inner = block.inner(area);
        f.render_widget(block, area);
        let para = Paragraph::new(Line::from(vec![
            Span::styled("• ", theme.muted_text()),
            Span::styled(hint, theme.muted_text()),
        ]));
        f.render_widget(para, pad_left(inner, 1));
    } else {
        let table = table.block(block);
        let mut state = TableState::default();
        state.select(Some(selected));
        f.render_stateful_widget(table, area, &mut state);
    }
}

/// Trim `n` columns from the left of a rect (for padding text off the border).
pub(super) fn pad_left(area: Rect, n: u16) -> Rect {
    let n = n.min(area.width);
    Rect {
        x: area.x + n,
        width: area.width - n,
        ..area
    }
}

/// Trim `n` columns from the right of a rect.
pub(super) fn pad_right(area: Rect, n: u16) -> Rect {
    let n = n.min(area.width);
    Rect {
        width: area.width - n,
        ..area
    }
}

/// The terminal display width of a string (double-width CJK/emoji count as 2, control chars as 0).
pub(super) fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Truncate `s` to at most `max` display columns, appending an ellipsis `…` when it doesn't fit
/// (rather than a hard mid-glyph cut). Width-correct: a truncated string plus the `…` never exceeds
/// `max` columns.
pub(super) fn truncate_ellipsis(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if display_width(s) <= max {
        return s.to_string();
    }
    // Reserve one column for the ellipsis, then take whole characters up to that budget.
    let budget = max.saturating_sub(1);
    let mut out = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > budget {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

pub(super) fn empty_repos_hint(app: &App) -> String {
    format!(
        "no {} repositories — press f to change the filter, r to refresh",
        app.repo_filter.label()
    )
}

pub(super) fn repo_label(t: &TaskRow) -> String {
    match (&t.repo_owner, &t.repo_name) {
        (Some(o), Some(n)) => format!("{o}/{n}"),
        _ => format!("repo#{}", t.repository_id),
    }
}

/// A `PR #12` / `issue #7` style target label.
pub(super) fn target_label(t: &TaskRow) -> String {
    let sigil = match t.target_type.as_str() {
        "pull_request" => "PR",
        "issue" => "issue",
        "" => "target",
        other => other,
    };
    format!("{sigil} #{}", t.target_id)
}

/// A human age like `3m`, `2h`, `5d`.
pub(super) fn age(created: OffsetDateTime) -> String {
    let secs = (crate::auth::now_unix() - created.unix_timestamp()).max(0);
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

/// Format an optional rfc3339 timestamp as `YYYY-MM-DD HH:MM`, or `—`.
pub(super) fn fmt_ts(ts: Option<OffsetDateTime>) -> String {
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
pub(super) fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    use ratatui::layout::{Constraint, Direction, Layout};
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
            base_sha: None,
            head_sha: None,
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
    fn fmt_ts_handles_none() {
        assert_eq!(fmt_ts(None), "—");
        assert!(fmt_ts(Some(OffsetDateTime::UNIX_EPOCH)).starts_with("1970-01-01"));
    }

    #[test]
    fn pad_helpers_never_overflow_on_tiny_rects() {
        let tiny = Rect {
            x: 0,
            y: 0,
            width: 0,
            height: 1,
        };
        // Must not panic / underflow when asked to pad more than the width.
        assert_eq!(pad_left(tiny, 5).width, 0);
        assert_eq!(pad_right(tiny, 5).width, 0);
    }

    #[test]
    fn truncate_ellipsis_fits_within_the_budget() {
        // Fits untouched.
        assert_eq!(truncate_ellipsis("hello", 10), "hello");
        assert_eq!(truncate_ellipsis("hello", 5), "hello");
        // Too long → ellipsis, and the result never exceeds the column budget.
        let out = truncate_ellipsis("j/k move · f filter · r refresh · q quit", 12);
        assert!(display_width(&out) <= 12, "stays within budget: {out:?}");
        assert!(out.ends_with('…'));
        // Degenerate budgets don't panic.
        assert_eq!(truncate_ellipsis("anything", 0), "");
        assert_eq!(display_width(&truncate_ellipsis("anything", 1)), 1);
    }
}
