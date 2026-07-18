//! The Run Detail page: a stacked meta panel and a review panel — joined with **manually collapsed
//! borders** (the lower panel drops its top border so it reads as one continuous frame). ratatui 0.30
//! could express this with `merge_borders` / `Spacing::Overlap`, but 0.29 lacks those, so we omit the
//! touching edge by hand. Run observability (the model's turns/reasoning) is Loki-only now (epic
//! #459) — the live-tailing transcript panel was removed.

use super::super::app::{App, DetailState};
use super::helpers::{fmt_ts, pad_left, repo_label, target_label, truncate_ellipsis};
use crate::api::TaskRow;
use crate::theme::{Theme, status_label};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Wrap};

/// The Run Detail page: a stacked meta panel and the review panel below it (which fills the rest of
/// the height).
pub(super) fn draw_detail(f: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let Some(d) = app.detail.as_ref() else {
        return;
    };

    // Meta 8 rows (6 inner k/v rows: the right column's sha/created/started/completed/duration/job).
    // On a failed/timed-out run we add a row (→9) so the error-detail line has its own row instead of
    // overwriting `job`. The review panel takes the rest; `Min(3)` guards a graceful degrade on short
    // terminals.
    let meta_height = match d.task.status.as_str() {
        "failed" | "timed_out" => 9,
        _ => 8,
    };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(meta_height), Constraint::Min(3)])
        .split(area);

    draw_detail_meta(f, rows[0], d, theme);
    draw_detail_review(f, rows[1], d, theme);
}

/// A short (7-char) task id for the header/meta.
fn short_id(id: uuid::Uuid) -> String {
    id.simple().to_string()[..8].to_string()
}

/// 7-char short SHA (or `—` when the side is absent — e.g. a non-PR run, or an older row).
fn short_sha(sha: Option<&str>) -> String {
    match sha {
        Some(s) if s.len() >= 7 => s[..7].to_string(),
        Some(s) => s.to_string(),
        None => "—".into(),
    }
}

/// The top meta panel: id, status, repo, target, kind, command, timestamps + duration, job, and (on
/// failure) the error detail. Full `Borders::ALL` — it's the top of the collapsed stack.
fn draw_detail_meta(f: &mut Frame, area: Rect, d: &DetailState, theme: &Theme) {
    let t = &d.task;
    let live_badge = detail_live_badge(d, theme);
    let mut title_spans = vec![
        Span::styled(
            format!("▐ Run {} ▌", short_id(t.id)),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
    ];
    title_spans.extend(live_badge);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.border))
        .style(Style::default().bg(theme.background))
        .title(Line::from(title_spans));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Two columns of key/value pairs so it stays compact. Width 10 so `completed` keeps a space.
    let kv = |k: &str, v: Span<'static>| -> Line<'static> {
        Line::from(vec![
            Span::styled(format!("{k:<10}"), theme.muted_text()),
            v,
        ])
    };
    let status_span = Span::styled(
        status_label(&t.status).to_string(),
        Style::default()
            .fg(theme.status_color(&t.status))
            .add_modifier(Modifier::BOLD),
    );

    let left = vec![
        kv("status", status_span),
        kv("repo", Span::styled(repo_label(t), theme.text())),
        kv("target", Span::styled(target_label(t), theme.text())),
        kv("kind", Span::styled(t.kind.clone(), theme.text())),
        kv(
            "command",
            Span::styled(
                if t.command_text.is_empty() {
                    "—".into()
                } else {
                    t.command_text.clone()
                },
                theme.text(),
            ),
        ),
    ];

    let dur = duration_label(t);
    let right = vec![
        kv(
            "sha",
            Span::styled(
                format!(
                    "{}→{}",
                    short_sha(t.base_sha.as_deref()),
                    short_sha(t.head_sha.as_deref())
                ),
                theme.muted_text(),
            ),
        ),
        kv(
            "created",
            Span::styled(fmt_ts(Some(t.created_at)), theme.muted_text()),
        ),
        kv(
            "started",
            Span::styled(fmt_ts(t.started_at), theme.muted_text()),
        ),
        kv(
            "completed",
            Span::styled(fmt_ts(t.completed_at), theme.muted_text()),
        ),
        kv("duration", Span::styled(dur, theme.text())),
    ];

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(52), Constraint::Percentage(48)])
        .split(pad_left(inner, 1));
    f.render_widget(Paragraph::new(left), cols[0]);

    // The right column also carries job_name, and the error detail if the run failed.
    let mut right = right;
    right.push(kv(
        "job",
        Span::styled(
            t.job_name.clone().unwrap_or_else(|| "—".into()),
            theme.muted_text(),
        ),
    ));
    f.render_widget(Paragraph::new(right), cols[1]);

    // On a failed/timed-out run, surface the error detail across the full inner width (last row).
    if matches!(t.status.as_str(), "failed" | "timed_out")
        && let Some(err) = t.error_detail.as_deref()
    {
        let err_area = Rect {
            y: inner.y + inner.height.saturating_sub(1),
            height: 1,
            ..pad_left(inner, 1)
        };
        let line = Line::from(vec![
            Span::styled(format!("{:<10}", "error"), theme.muted_text()),
            Span::styled(
                truncate_ellipsis(err, err_area.width.saturating_sub(10) as usize),
                Style::default()
                    .fg(theme.error)
                    .add_modifier(Modifier::BOLD),
            ),
        ]);
        f.render_widget(Paragraph::new(line), err_area);
    }
}

/// The `● live` / `● done` / `● failed` badge shown in the detail header + meta title.
fn detail_live_badge(d: &DetailState, theme: &Theme) -> Vec<Span<'static>> {
    let (color, label) = match (d.live, d.task.status.as_str()) {
        (true, _) => (theme.info, "live"),
        (false, "failed" | "timed_out") => (theme.error, "failed"),
        (false, "cancelled") => (theme.muted, "cancelled"),
        (false, _) => (theme.success, "done"),
    };
    vec![
        Span::styled("● ", Style::default().fg(color)),
        Span::styled(label, Style::default().fg(color)),
    ]
}

/// The review panel — collapsed onto the meta panel above (drops its TOP border) and filling the rest
/// of the page height. Shows the wrapped summary + a colored finding tally + `review_url`, an inline
/// "no review" line, or a permission notice.
fn draw_detail_review(f: &mut Frame, area: Rect, d: &DetailState, theme: &Theme) {
    // Borders::ALL & !Borders::TOP collapses the shared edge with the meta panel above. (0.30's
    // merge_borders/Spacing::Overlap would express this directly; 0.29 does it by omission.)
    let block = Block::default()
        .borders(Borders::ALL & !Borders::TOP)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.border))
        .style(Style::default().bg(theme.background))
        .title(Line::from(Span::styled(
            "▐ Review ▌",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let inner = pad_left(inner, 1);

    // Dispatch on the three-way review state: permission-denied, loaded-with-a-review,
    // loaded-with-none, still-loading.
    let lines: Vec<Line> = match (d.permission_denied, &d.review, d.review_loaded) {
        (true, _, _) => vec![Line::from(Span::styled(
            "insufficient permission (review:read) to view run detail",
            Style::default().fg(theme.warning),
        ))],
        (false, Some(r), _) => {
            let tally = Line::from(vec![
                Span::styled("inline ", theme.muted_text()),
                Span::styled(
                    r.inline_count.to_string(),
                    Style::default().fg(theme.info).add_modifier(Modifier::BOLD),
                ),
                Span::styled("  ·  deferred ", theme.muted_text()),
                Span::styled(
                    r.deferred_count.to_string(),
                    Style::default()
                        .fg(theme.warning)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("  ·  out-of-scope ", theme.muted_text()),
                Span::styled(
                    r.out_of_scope_count.to_string(),
                    Style::default()
                        .fg(theme.muted)
                        .add_modifier(Modifier::BOLD),
                ),
            ]);
            // The full summary wraps to fill the panel; the tally + url follow.
            let mut v = vec![
                Line::from(Span::styled(r.summary.clone(), theme.text())),
                tally,
            ];
            if let Some(url) = &r.review_url {
                v.push(Line::from(Span::styled(
                    truncate_ellipsis(url, inner.width as usize),
                    Style::default().fg(theme.secondary),
                )));
            }
            v
        }
        (false, None, true) => vec![Line::from(Span::styled(
            "• no review recorded (yet)",
            theme.muted_text(),
        ))],
        (false, None, false) => vec![Line::from(Span::styled(
            "• loading review…",
            theme.muted_text(),
        ))],
    };
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), inner);
}

/// A human duration for a run: completed − started, else now − started while running, else `—`.
fn duration_label(t: &TaskRow) -> String {
    let start = match t.started_at {
        Some(s) => s.unix_timestamp(),
        None => return "—".into(),
    };
    let end = match t.completed_at {
        Some(c) => c.unix_timestamp(),
        None => crate::auth::now_unix(),
    };
    let secs = (end - start).max(0);
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m{:02}s", s / 60, s % 60),
        s => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_sha_truncates_to_seven_and_falls_back() {
        assert_eq!(short_sha(Some("a1b2c3d4e5f6")), "a1b2c3d");
        assert_eq!(
            short_sha(Some("abc")),
            "abc",
            "shorter-than-7 passes through"
        );
        assert_eq!(short_sha(None), "—", "absent side renders as —");
    }
}
