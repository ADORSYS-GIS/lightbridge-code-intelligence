//! Pure rendering: given an [`App`], draw the current frame. No I/O, no state mutation.
//!
//! The look is k9s- and opencode-inspired: a fixed header (logo + context block + keymenu), a
//! pill-tab bar, a bordered content table with semantic status coloring and an accent selection
//! cursor, and a status/footer bar with a live spinner + toast. Every color comes from the active
//! [`Theme`] — there are no hardcoded `Color::`s here. The governing discipline: accent only for
//! interactive/selected elements, status in semantic colors, metadata muted, most text in the
//! default foreground.

use super::app::{App, DetailState, ToastKind, View};
use crate::api::TaskRow;
use crate::theme::{ButtonKind, Theme, status_label};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Table, TableState,
};
use time::OffsetDateTime;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// The braille spinner cycle (8 frames), width-1 each.
const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

/// Minimum usable terminal size; below this we render a single graceful line instead of a clipped
/// mess.
const MIN_WIDTH: u16 = 60;
const MIN_HEIGHT: u16 = 15;

/// Draw one frame.
pub fn draw(f: &mut Frame, app: &App) {
    let theme = app.theme();
    let area = f.area();

    // Paint the whole surface with the theme background first (a no-op for the `terminal` theme,
    // which uses `Color::Reset`).
    f.render_widget(
        Block::default().style(theme.text().bg(theme.background)),
        area,
    );

    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        draw_too_small(f, area, &theme);
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(6), // header (logo + context + keymenu)
            Constraint::Length(1), // tab bar
            Constraint::Min(3),    // framed content
            Constraint::Length(1), // status / footer bar
        ])
        .split(area);

    draw_header(f, chunks[0], app, &theme);
    draw_tabs(f, chunks[1], app, &theme);
    match app.view {
        View::Repositories => draw_repositories(f, chunks[2], app, &theme),
        View::Runs => draw_runs(f, chunks[2], app, &theme),
        View::Detail => draw_detail(f, chunks[2], app, &theme),
    }
    draw_status(f, chunks[3], app, &theme);

    if let Some(confirm) = &app.confirm {
        draw_confirm(f, confirm, &theme);
    }
    if app.show_help {
        draw_help(f, &theme);
    }
}

/// The graceful fallback for a terminal too small to lay out.
fn draw_too_small(f: &mut Frame, area: Rect, theme: &Theme) {
    let msg = Paragraph::new(vec![
        Line::from(Span::styled(
            "terminal too small",
            Style::default()
                .fg(theme.error)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!("need ≥ {MIN_WIDTH}×{MIN_HEIGHT}"),
            theme.muted_text(),
        )),
    ])
    .alignment(Alignment::Center);
    f.render_widget(msg, area);
}

// --- header -------------------------------------------------------------------------------------

/// The header: a `▍ LCI` logo on the left, a k9s-style context block in the center, and a keymenu on
/// the right — all on the surface fill.
fn draw_header(f: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    f.render_widget(Block::default().style(theme.surface_style()), area);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(26), // logo
            Constraint::Min(24),    // context block
            Constraint::Length(24), // keymenu
        ])
        .split(area);

    draw_logo(f, cols[0], theme);
    draw_context(f, cols[1], app, theme);
    draw_keymenu(f, cols[2], app, theme);
}

/// The compact `▍ LCI` wordmark + subtitle. Top-aligned so the wordmark sits on the header's first
/// row, level with `Host:` and the keymenu (an earlier leading blank made the top row look lopsided).
fn draw_logo(f: &mut Frame, area: Rect, theme: &Theme) {
    let brand = Style::default()
        .fg(theme.brand)
        .add_modifier(Modifier::BOLD);
    let lines = vec![
        Line::from(vec![Span::styled("▍ ", brand), Span::styled("LCI", brand)]),
        Line::from(Span::styled("Lightbridge Code", theme.muted_text())),
        Line::from(Span::styled("Intelligence", theme.muted_text())),
    ];
    f.render_widget(
        Paragraph::new(lines).style(theme.surface_style()),
        pad_left(area, 1),
    );
}

/// The k9s-style `key: value` context block.
fn draw_context(f: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let identity = app.me.as_ref().map(|m| m.identity()).unwrap_or("unknown");
    let perms = app
        .me
        .as_ref()
        .map(|m| perm_summary(&m.permissions))
        .unwrap_or_else(|| "—".into());

    let kv = |k: &str, v: Span<'static>| -> Line<'static> {
        Line::from(vec![Span::styled(format!("{k:<7}"), theme.muted_text()), v])
    };

    // Connection dot: ok (info) normally, warning if a re-auth is pending.
    let (dot_color, dot_label) = if app.reauth_needed {
        (theme.warning, "reauth needed")
    } else {
        (theme.success, "connected")
    };

    let lines = vec![
        kv("Host:", Span::styled(app.api_host.clone(), theme.text())),
        kv(
            "User:",
            Span::styled(
                identity.to_string(),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
        ),
        kv("Perms:", Span::styled(perms, theme.text())),
        Line::from(vec![
            Span::styled(format!("{:<7}", "Token:"), theme.muted_text()),
            token_span(app, theme),
            Span::raw("   "),
            Span::styled("● ", Style::default().fg(dot_color)),
            Span::styled(dot_label.to_string(), theme.muted_text()),
        ]),
    ];
    f.render_widget(Paragraph::new(lines).style(theme.surface_style()), area);
}

/// The right-aligned keymenu: `<key> label`, key in accent. Actions the caller can't perform are
/// dimmed.
fn draw_keymenu(f: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let entry = |key: &str, label: &str, enabled: bool| -> Line<'static> {
        let (key_style, label_style) = if enabled {
            (
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
                theme.text(),
            )
        } else {
            (theme.muted_text(), theme.muted_text())
        };
        Line::from(vec![
            Span::styled(format!("<{key}> "), key_style),
            Span::styled(label.to_string(), label_style),
        ])
    };

    // Gate approve/deny/cancel on the token's capabilities.
    let a = app.can_approve();
    let d = app.can_deny();
    let c = app.can_cancel();

    // On the detail page, surface its own keys (scroll/back/mouse) instead of the list actions.
    let lines = if app.view == View::Detail {
        vec![
            entry("↵/l", "open", true),
            entry("Esc", "back", true),
            entry("G", "tail", true),
            entry("m", "mouse", true),
            entry("?", "help", true),
        ]
    } else {
        vec![
            entry("a", "approve", a),
            entry("d", "deny", d),
            entry("c", "cancel", c),
            entry("m", "mouse", true),
            entry("?", "help", true),
        ]
    };
    f.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Right)
            .style(theme.surface_style()),
        pad_right(area, 1),
    );
}

// --- tab bar ------------------------------------------------------------------------------------

/// The pill-tab bar: active tab gets an accent background, inactive tabs muted on the surface.
fn draw_tabs(f: &mut Frame, area: Rect, app: &App, theme: &Theme) {
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

// --- content tables -----------------------------------------------------------------------------

/// Repositories table.
fn draw_repositories(f: &mut Frame, area: Rect, app: &App, theme: &Theme) {
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

/// Runs table.
fn draw_runs(f: &mut Frame, area: Rect, app: &App, theme: &Theme) {
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

// --- run detail page ----------------------------------------------------------------------------

/// The Run Detail page: a stacked meta panel, a small review panel, and the large live-tailing
/// transcript panel — joined with **manually collapsed borders** (the lower panel drops its top
/// border so it reads as one continuous frame). ratatui 0.30 could express this with `merge_borders`
/// / `Spacing::Overlap`, but 0.29 lacks those, so we omit the touching edge by hand.
fn draw_detail(f: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let Some(d) = app.detail.as_ref() else {
        return;
    };

    // Meta 8 rows (6 inner k/v rows: the right column's sha/created/started/completed/duration/job).
    // On a failed/timed-out run we add a row (→9) so the error-detail line has its own row instead of
    // overwriting `job`. Review 4 rows; transcript takes the rest. `Min(3)` guards a graceful degrade
    // on short terminals — at the 80×24 review size the non-error case sums exactly to the content area.
    let meta_height = if matches!(d.task.status.as_str(), "failed" | "timed_out") {
        9
    } else {
        8
    };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(meta_height),
            Constraint::Length(4),
            Constraint::Min(3),
        ])
        .split(area);

    draw_detail_meta(f, rows[0], d, theme);
    draw_detail_review(f, rows[1], d, theme);
    draw_detail_transcript(f, rows[2], d, theme);
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
    let (color, label) = if d.live {
        (theme.info, "live")
    } else if matches!(d.task.status.as_str(), "failed" | "timed_out") {
        (theme.error, "failed")
    } else if d.task.status == "cancelled" {
        (theme.muted, "cancelled")
    } else {
        (theme.success, "done")
    };
    vec![
        Span::styled("● ", Style::default().fg(color)),
        Span::styled(label.to_string(), Style::default().fg(color)),
    ]
}

/// The middle review panel — collapsed onto the meta panel above (drops its TOP border). Shows the
/// summary + a colored finding tally + `review_url`, an inline "no review" line, or a permission
/// notice.
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

    let lines: Vec<Line> = if d.permission_denied {
        vec![Line::from(Span::styled(
            "insufficient permission (review:read) to view run detail",
            Style::default().fg(theme.warning),
        ))]
    } else if let Some(r) = &d.review {
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
        let mut v = vec![
            Line::from(Span::styled(
                truncate_ellipsis(&r.summary, inner.width as usize),
                theme.text(),
            )),
            tally,
        ];
        if let Some(url) = &r.review_url {
            v.push(Line::from(Span::styled(
                truncate_ellipsis(url, inner.width as usize),
                Style::default().fg(theme.secondary),
            )));
        }
        v
    } else if d.review_loaded {
        vec![Line::from(Span::styled(
            "• no review recorded (yet)",
            theme.muted_text(),
        ))]
    } else {
        vec![Line::from(Span::styled(
            "• loading review…",
            theme.muted_text(),
        ))]
    };
    f.render_widget(Paragraph::new(lines), inner);
}

/// The large transcript panel (the "log tail"). Renders wrapped turns newest-at-bottom with a
/// vertical scrollbar, records the measured geometry back to state (for autoscroll), and shows a
/// `▼ N new` badge in the title when the operator has scrolled up during a live tail.
fn draw_detail_transcript(f: &mut Frame, area: Rect, d: &DetailState, theme: &Theme) {
    // Title carries the turn count and, when scrolled up mid-tail, the unseen-turn badge.
    let mut title_spans = vec![
        Span::styled(
            "▐ Transcript ▌",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {} ", d.transcript.len()),
            Style::default().fg(theme.on_accent()).bg(theme.secondary),
        ),
    ];
    if d.new_since_scroll > 0 {
        title_spans.push(Span::raw(" "));
        title_spans.push(Span::styled(
            format!("▼ {} new", d.new_since_scroll),
            Style::default().fg(theme.info).add_modifier(Modifier::BOLD),
        ));
    } else if d.live && d.autoscroll {
        title_spans.push(Span::raw(" "));
        title_spans.push(Span::styled("tailing", theme.muted_text()));
    }

    let block = Block::default()
        .borders(Borders::ALL & !Borders::TOP)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.border))
        .style(Style::default().bg(theme.background))
        .title(Line::from(title_spans));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Reserve one column on the right for the scrollbar track.
    let text_area = Rect {
        width: inner.width.saturating_sub(1),
        ..pad_left(inner, 1)
    };

    // Build wrapped lines. Reserve the wrap width so we can measure total content height for the
    // scrollbar + autoscroll. (We pre-wrap ourselves rather than trusting Paragraph's internal wrap
    // count, which it doesn't expose.)
    let wrap_width = text_area.width.max(1) as usize;
    let lines = transcript_lines(d, wrap_width, theme);
    let total = lines.len() as u16;

    // Hand the measured geometry back to state (Cell write during immutable draw); the loop pins
    // autoscroll on the next turn.
    d.record_geometry(total, text_area.height);

    if d.permission_denied {
        // The full `review:read` explanation is shown once, in the review panel above; here we keep a
        // short, distinct hidden-marker so the two panels don't stack the identical sentence.
        let para = Paragraph::new(Line::from(Span::styled(
            "— transcript hidden (needs review:read) —",
            theme.muted_text(),
        )));
        f.render_widget(para, text_area);
        return;
    }
    if d.transcript.is_empty() {
        let hint = if d.transcript_loaded {
            if d.live {
                "• no turns yet — the agent hasn't logged activity (tailing…)"
            } else {
                "• no transcript recorded for this run"
            }
        } else {
            "• loading transcript…"
        };
        let para = Paragraph::new(Line::from(Span::styled(hint, theme.muted_text())));
        f.render_widget(para, text_area);
        return;
    }

    let offset = d.scroll.min(total.saturating_sub(text_area.height));
    let para = Paragraph::new(lines)
        .style(theme.text())
        .scroll((offset, 0));
    f.render_widget(para, text_area);

    // The scrollbar sits in the reserved right column of the inner area.
    let mut sb_state = ScrollbarState::new(total as usize)
        .viewport_content_length(text_area.height as usize)
        .position(offset as usize);
    let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
        .begin_symbol(None)
        .end_symbol(None)
        .thumb_style(Style::default().fg(theme.accent))
        .track_style(theme.muted_text());
    f.render_stateful_widget(scrollbar, inner, &mut sb_state);
}

/// Flatten the transcript into styled, pre-wrapped display lines (newest at the bottom). Each turn is
/// a compact header line (seq · role · model · ↑prompt ↓completion) followed by wrapped content
/// and/or a tool-call summary. Pre-wrapping lets us measure total height for the scrollbar.
fn transcript_lines(d: &DetailState, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut out: Vec<Line> = Vec::new();
    for turn in &d.transcript {
        // Header: seq + role (color-keyed) + optional model + token counts.
        let (role_color, role_label) = match turn.role.as_str() {
            "assistant" => (theme.accent, "assistant"),
            "tool" => (theme.secondary, "tool"),
            "user" => (theme.info, "user"),
            other => (theme.muted, other),
        };
        let mut header = vec![
            Span::styled(format!("#{:<3}", turn.seq), theme.muted_text()),
            Span::styled(
                role_label.to_string(),
                Style::default().fg(role_color).add_modifier(Modifier::BOLD),
            ),
        ];
        if let Some(model) = &turn.model {
            header.push(Span::styled(format!("  {model}"), theme.muted_text()));
        }
        if turn.prompt_tokens.is_some() || turn.completion_tokens.is_some() {
            let p = turn.prompt_tokens.unwrap_or(0);
            let c = turn.completion_tokens.unwrap_or(0);
            header.push(Span::styled(format!("  ↑{p} ↓{c}"), theme.muted_text()));
        }
        out.push(Line::from(header));

        // A tool call: name + a short arg summary.
        if let Some(name) = &turn.tool_name {
            let summary = tool_call_summary(turn.tool_calls.as_ref());
            let mut spans = vec![
                Span::styled("  ⚙ ", theme.muted_text()),
                Span::styled(name.clone(), Style::default().fg(theme.secondary)),
            ];
            if !summary.is_empty() {
                spans.push(Span::styled(format!(" {summary}"), theme.muted_text()));
            }
            for line in wrap_spans(spans, width) {
                out.push(line);
            }
        }

        // The content, wrapped.
        if let Some(content) = &turn.content {
            for raw in content.lines() {
                for line in wrap_plain(raw, width, theme.text()) {
                    out.push(line);
                }
            }
        }

        out.push(Line::from(""));
    }
    // Drop the trailing blank so the last turn hugs the bottom for a clean tail.
    if matches!(out.last(), Some(l) if l.spans.is_empty()) {
        out.pop();
    }
    out
}

/// A compact one-line summary of a tool_calls JSON blob (keys or a short serialization), truncated.
fn tool_call_summary(tool_calls: Option<&serde_json::Value>) -> String {
    let Some(v) = tool_calls else {
        return String::new();
    };
    // Prefer an `args`/`arguments` object's keys; else a compact JSON, truncated to keep it a line.
    let compact = v
        .get("args")
        .or_else(|| v.get("arguments"))
        .map(summarize_json_keys)
        .unwrap_or_else(|| summarize_json_keys(v));
    truncate_ellipsis(&compact, 80)
}

/// Summarize a JSON value as `{a, b, c}` (object keys) or its compact string form.
fn summarize_json_keys(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Object(map) => {
            let keys: Vec<&str> = map.keys().map(String::as_str).collect();
            format!("{{{}}}", keys.join(", "))
        }
        other => other.to_string(),
    }
}

/// Wrap a plain string into styled `Line`s at `width` columns (whole-word-ish: splits at the column
/// budget, never mid-glyph). An empty budget yields one empty line.
fn wrap_plain(s: &str, width: usize, style: Style) -> Vec<Line<'static>> {
    if width == 0 || s.is_empty() {
        return vec![Line::from(Span::styled(s.to_string(), style))];
    }
    let mut lines = Vec::new();
    let mut cur = String::new();
    let mut used = 0usize;
    for ch in s.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > width {
            lines.push(Line::from(Span::styled(std::mem::take(&mut cur), style)));
            used = 0;
        }
        cur.push(ch);
        used += w;
    }
    lines.push(Line::from(Span::styled(cur, style)));
    lines
}

/// Wrap a run of spans onto as many lines as needed at `width`, by their combined text. Styling is
/// preserved per-span but a span that overflows is split at the column budget.
fn wrap_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Line<'static>> {
    // Simple approach: flatten to (text, style) pairs, then greedily pack.
    let mut lines: Vec<Line> = Vec::new();
    let mut cur: Vec<Span> = Vec::new();
    let mut used = 0usize;
    for span in spans {
        let style = span.style;
        for ch in span.content.chars() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(0);
            if width > 0 && used + w > width {
                lines.push(Line::from(std::mem::take(&mut cur)));
                used = 0;
            }
            // Append to the last span if same style, else push a new one.
            if let Some(last) = cur.last_mut()
                && last.style == style
            {
                last.content.to_mut().push(ch);
                used += w;
                continue;
            }
            cur.push(Span::styled(ch.to_string(), style));
            used += w;
        }
    }
    if !cur.is_empty() {
        lines.push(Line::from(cur));
    }
    if lines.is_empty() {
        lines.push(Line::from(""));
    }
    lines
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
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Render the table, or an inline muted status line inside the same bordered block when there are no
/// rows (empty states are inline status lines, not centered placards).
#[allow(clippy::too_many_arguments)]
fn render_table_or_empty(
    f: &mut Frame,
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

// --- status / footer bar ------------------------------------------------------------------------

/// The full-width status bar: LEFT filter + spinner, CENTER toast, RIGHT key hint.
///
/// The three segments are sized to their content — the side segments take exactly what they need
/// (clamped so they never starve each other), and the center toast takes the remainder — instead of
/// rigid percentages that hard-cut a longer hint/toast at 80 cols (gemini). Any segment that still
/// can't fit is truncated with an ellipsis rather than clipped mid-glyph.
fn draw_status(f: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    f.render_widget(Block::default().style(theme.surface_style()), area);

    // LEFT text: " ⠹ filter: pending" (spinner only while loading). On the detail page it shows the
    // mouse-capture + tail state instead of a filter.
    let filter = match app.view {
        View::Repositories => format!("filter: {}", app.repo_filter.label()),
        View::Runs => format!(
            "filter: {}",
            if app.runs_active_only {
                "active"
            } else {
                "all"
            }
        ),
        View::Detail => {
            let mouse = if app.mouse_enabled {
                "mouse:on"
            } else {
                "mouse:off"
            };
            let tail = match app.detail.as_ref() {
                Some(d) if d.live && d.autoscroll => "tail",
                Some(d) if d.live => "held",
                _ => "static",
            };
            format!("{mouse} · {tail}")
        }
    };
    let spinner = if app.loading {
        format!("{} ", SPINNER[app.spinner_frame % SPINNER.len()])
    } else {
        String::new()
    };
    let left_text = format!(" {spinner}{filter}");

    // RIGHT text: the key hint (trailing space keeps it off the edge).
    let hint = match app.view {
        View::Repositories => "j/k move · f filter · r refresh · q quit ",
        View::Runs => "↵ open · j/k move · f active/all · r refresh · q quit ",
        View::Detail => "j/k scroll · G bottom · m mouse · r refresh · Esc back ",
    };

    // Give the side segments what their content needs, but never more than ~45% each so a very long
    // one can't crowd the other out; the center takes whatever remains for the toast.
    let side_cap = (area.width as usize * 45 / 100).max(1);
    let left_w = display_width(&left_text).min(side_cap) as u16;
    let right_w = display_width(hint).min(side_cap) as u16;
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length(left_w),
            Constraint::Min(0),
            Constraint::Length(right_w),
        ])
        .split(area);

    // LEFT.
    let left_line = Line::from(Span::styled(
        truncate_ellipsis(&left_text, cols[0].width as usize),
        theme.muted_text(),
    ));
    f.render_widget(
        Paragraph::new(left_line).style(theme.surface_style()),
        cols[0],
    );

    // CENTER: the latest toast, semantic-colored, centered, truncated to the remaining width.
    if let Some(toast) = &app.toast {
        let color = match toast.kind {
            ToastKind::Info => theme.info,
            ToastKind::Success => theme.success,
            ToastKind::Error => theme.error,
        };
        let glyph = match toast.kind {
            ToastKind::Info => "•",
            ToastKind::Success => "✓",
            ToastKind::Error => "✗",
        };
        let text = truncate_ellipsis(&format!("{glyph} {}", toast.text), cols[1].width as usize);
        let line = Line::from(Span::styled(
            text,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ))
        .alignment(Alignment::Center);
        f.render_widget(Paragraph::new(line).style(theme.surface_style()), cols[1]);
    }

    // RIGHT: the key hint, right-aligned, truncated to its segment.
    let right_line = Line::from(Span::styled(
        truncate_ellipsis(hint, cols[2].width as usize),
        theme.muted_text(),
    ))
    .alignment(Alignment::Right);
    f.render_widget(
        Paragraph::new(right_line).style(theme.surface_style()),
        cols[2],
    );
}

// --- overlays -----------------------------------------------------------------------------------

/// The centered confirm dialog with two button-styled choices.
fn draw_confirm(f: &mut Frame, confirm: &super::app::Confirm, theme: &Theme) {
    let area = centered_rect(58, 34, f.area());
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.border_focus))
        .style(Style::default().bg(theme.surface))
        .title(Span::styled(
            " Confirm ",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Split into the message area and a bottom row for the buttons.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(pad_left(pad_right(inner, 1), 1));

    let text = Text::from(vec![
        Line::from(""),
        Line::from(Span::styled(
            confirm.prompt.clone(),
            Style::default()
                .fg(theme.foreground)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(confirm.detail.clone(), theme.muted_text())),
    ]);
    f.render_widget(
        Paragraph::new(text)
            .alignment(Alignment::Center)
            .style(Style::default().bg(theme.surface)),
        rows[0],
    );

    // Buttons: the affirmative (verb) and a Cancel. The focused one gets ›‹ markers + a solid fill.
    let affirmative = button_span(
        &confirm.verb,
        confirm.confirm_focused,
        confirm.verb_kind,
        theme,
    );
    let cancel = button_span(
        "Cancel",
        !confirm.confirm_focused,
        ButtonKind::Neutral,
        theme,
    );
    let mut btn_spans = vec![Span::raw(" ")];
    btn_spans.extend(affirmative);
    btn_spans.push(Span::styled("   ", Style::default().bg(theme.surface)));
    btn_spans.extend(cancel);
    f.render_widget(
        Paragraph::new(Line::from(btn_spans).alignment(Alignment::Center))
            .style(Style::default().bg(theme.surface)),
        rows[1],
    );
}

/// A single button rendered as `‹ Label ›` (focused) / `  Label  ` (unfocused).
fn button_span(label: &str, focused: bool, kind: ButtonKind, theme: &Theme) -> Vec<Span<'static>> {
    let style = theme.button(focused, kind);
    let text = if focused {
        format!("› {label} ‹")
    } else {
        format!("  {label}  ")
    };
    vec![Span::styled(text, style)]
}

/// The help overlay: a two-column keybinding grid with accent section headers.
fn draw_help(f: &mut Frame, theme: &Theme) {
    let area = centered_rect(66, 80, f.area());
    f.render_widget(Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.border_focus))
        .style(Style::default().bg(theme.surface))
        .title(Span::styled(
            " Help · lci ",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let section = |title: &str| -> Line<'static> {
        Line::from(Span::styled(
            title.to_string(),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ))
    };
    let row = |key: &str, label: &str| -> Line<'static> {
        Line::from(vec![
            Span::styled(
                format!("  {key:<14}"),
                Style::default()
                    .fg(theme.secondary)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(label.to_string(), theme.text()),
        ])
    };

    let lines = vec![
        section("Navigation"),
        row("Tab / 1 / 2", "switch view (Repositories / Runs)"),
        row("↑/↓ or j/k", "move the selection"),
        row("↵ / l / →", "open the selected run's detail page"),
        row("r / f", "refresh · cycle filter"),
        section("Run detail (log tail)"),
        row("Esc / h / ←", "back to the Runs list"),
        row("j/k · PgUp/Dn", "scroll a line · a page"),
        row("g / G", "top · bottom (re-engages the live tail)"),
        section("Actions"),
        row("a / d / c", "approve · deny (purges index) · cancel run"),
        section("Appearance & misc"),
        row("t", "cycle the color theme"),
        row("m", "toggle mouse capture (off = native text-select)"),
        row("? / q", "toggle this help · quit"),
        Line::from(""),
        Line::from(Span::styled(
            "  Actions are gated by your token permissions.",
            theme.muted_text(),
        )),
    ];
    f.render_widget(
        Paragraph::new(lines).style(Style::default().bg(theme.surface)),
        pad_left(inner, 1),
    );
}

// --- small helpers ------------------------------------------------------------------------------

/// A k9s-style content title: `▐ Name ▌` + a count badge + an optional filter chip.
fn title_line(
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
fn status_chip(label: &str, theme: &Theme) -> Vec<Span<'static>> {
    vec![Span::styled(
        format!("[{label}]"),
        Style::default()
            .fg(theme.warning)
            .add_modifier(Modifier::BOLD),
    )]
}

/// A bordered content block carrying a rich title line.
fn bordered(title: Line<'static>, theme: &Theme) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(theme.border))
        .style(Style::default().bg(theme.background))
        .title(title)
}

/// The token expiry countdown span, colored by urgency (warning under 2 min).
fn token_span(app: &App, theme: &Theme) -> Span<'static> {
    let Some(exp) = app.token_expires_at else {
        return Span::styled("…", theme.muted_text());
    };
    let now = crate::auth::now_unix();
    let remaining = exp - now;
    if remaining <= 0 {
        return Span::styled(
            "expired",
            Style::default()
                .fg(theme.error)
                .add_modifier(Modifier::BOLD),
        );
    }
    let mins = remaining / 60;
    let secs = remaining % 60;
    let color = if remaining < 120 {
        theme.warning
    } else {
        theme.foreground
    };
    Span::styled(format!("{mins}m{secs:02}s"), Style::default().fg(color))
}

/// Trim `n` columns from the left of a rect (for padding text off the border).
fn pad_left(area: Rect, n: u16) -> Rect {
    let n = n.min(area.width);
    Rect {
        x: area.x + n,
        width: area.width - n,
        ..area
    }
}

/// Trim `n` columns from the right of a rect.
fn pad_right(area: Rect, n: u16) -> Rect {
    let n = n.min(area.width);
    Rect {
        width: area.width - n,
        ..area
    }
}

/// The terminal display width of a string (double-width CJK/emoji count as 2, control chars as 0).
fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Truncate `s` to at most `max` display columns, appending an ellipsis `…` when it doesn't fit
/// (rather than a hard mid-glyph cut). Width-correct: a truncated string plus the `…` never exceeds
/// `max` columns.
fn truncate_ellipsis(s: &str, max: usize) -> String {
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

/// A compact permissions summary (the action verbs the operator holds).
fn perm_summary(perms: &[String]) -> String {
    let mut verbs = Vec::new();
    for cap in ["repo:approve", "repo:deny", "task:cancel"] {
        if perms.iter().any(|p| p == cap) {
            verbs.push(cap.rsplit(':').next().unwrap_or(cap));
        }
    }
    if verbs.is_empty() {
        format!("{} (read-only)", perms.len())
    } else {
        verbs.join(" / ")
    }
}

fn empty_repos_hint(app: &App) -> String {
    format!(
        "no {} repositories — press f to change the filter, r to refresh",
        app.repo_filter.label()
    )
}

fn repo_label(t: &TaskRow) -> String {
    match (&t.repo_owner, &t.repo_name) {
        (Some(o), Some(n)) => format!("{o}/{n}"),
        _ => format!("repo#{}", t.repository_id),
    }
}

/// A `PR #12` / `issue #7` style target label.
fn target_label(t: &TaskRow) -> String {
    let sigil = match t.target_type.as_str() {
        "pull_request" => "PR",
        "issue" => "issue",
        "" => "target",
        other => other,
    };
    format!("{sigil} #{}", t.target_id)
}

/// A human age like `3m`, `2h`, `5d`.
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

/// Format an optional rfc3339 timestamp as `YYYY-MM-DD HH:MM`, or `—`.
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
    use crate::theme::ThemeKind;

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
    fn short_sha_truncates_to_seven_and_falls_back() {
        assert_eq!(short_sha(Some("a1b2c3d4e5f6")), "a1b2c3d");
        assert_eq!(
            short_sha(Some("abc")),
            "abc",
            "shorter-than-7 passes through"
        );
        assert_eq!(short_sha(None), "—", "absent side renders as —");
    }

    /// Draw an [`App`] in the detail view to a plain-text buffer (styling dropped), for asserting the
    /// meta/error layout without the `--render` seed screens. Uses the real `open_detail` path.
    fn draw_detail_to_string(task: TaskRow, w: u16, h: u16) -> String {
        use crate::api::{Claims, Me};
        use crate::tui::app::{App, View};
        use ratatui::{Terminal, backend::TestBackend};
        let me = Me {
            claims: Claims {
                sub: "s".into(),
                email: None,
                preferred_username: Some("op".into()),
                name: None,
                exp: Some(crate::auth::now_unix() + 300),
            },
            permissions: vec!["review:read".into(), "task:read".into()],
        };
        let mut app = App::new(
            me,
            "api.test".into(),
            crate::auth::now_unix() + 300,
            Some("rt".into()),
            ThemeKind::Midnight,
        );
        // Open the detail page through the real path: a selected Runs row → open_detail.
        app.set_view(View::Runs);
        app.runs_active_only = false; // so a terminal (failed) task is visible + selectable
        app.set_tasks(vec![task]);
        app.open_detail();
        assert_eq!(app.view, View::Detail, "detail opened via the real path");
        if let Some(d) = app.detail.as_mut() {
            d.review_loaded = true;
            d.transcript_loaded = true;
        }
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal.draw(|f| draw(f, &app)).expect("draw");
        let buffer = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..h {
            for x in 0..w {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn failed_run_shows_error_without_hiding_the_job_row() {
        let mut t = task("failed", "pull_request", 128);
        t.job_name = Some("review-4d0e".into());
        t.error_detail = Some("agent runner exited 137 (OOM-killed)".into());
        t.base_sha = Some("a1b2c3d4e5".into());
        t.head_sha = Some("e4f5a6b7c8".into());
        let s = draw_detail_to_string(t, 100, 26);
        // The meta panel grew by a row on failure, so BOTH the job and the error line are visible.
        assert!(s.contains("review-4d0e"), "job row not overwritten:\n{s}");
        assert!(s.contains("OOM-killed"), "error detail shown:\n{s}");
        // And the SHAs render (7-char short) rather than the old placeholder.
        assert!(s.contains("a1b2c3d→e4f5a6b"), "short SHAs rendered:\n{s}");
    }

    #[test]
    fn repo_label_falls_back_to_id() {
        let mut t = task("running", "pull_request", 1);
        t.repo_owner = None;
        assert_eq!(repo_label(&t), "repo#3");
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

    #[test]
    fn status_bar_hint_is_not_hard_cut_at_80_cols() {
        use crate::render::{Screen, render_to_string};
        use crate::theme::ThemeKind;
        // The old rigid 30/40/30 split hard-cut the hint; now the last visible glyph before the edge
        // is an ellipsis, never a mid-word slice.
        let s = render_to_string(Screen::Repos, 80, 24, ThemeKind::Midnight);
        let last = s.lines().last().unwrap_or_default();
        assert!(
            last.contains('…'),
            "narrow status bar ends the hint with an ellipsis, got: {last:?}"
        );
    }

    #[test]
    fn draws_every_screen_at_many_sizes_without_panicking() {
        use crate::render::{Screen, render_to_string};
        let screens = [
            Screen::Repos,
            Screen::Runs,
            Screen::Detail,
            Screen::Transcript,
            Screen::Confirm,
            Screen::Help,
            Screen::Empty,
            Screen::TooSmall,
        ];
        // A spread of sizes including the too-small regime and both required review sizes.
        let sizes = [(80, 24), (120, 40), (60, 15), (40, 10), (200, 60)];
        for screen in screens {
            for theme in [ThemeKind::Midnight, ThemeKind::Terminal, ThemeKind::Nord] {
                for (w, h) in sizes {
                    // Just exercising the render path — a panic here fails the test.
                    let out = render_to_string(screen, w, h, theme);
                    assert!(!out.is_empty(), "{screen:?} @ {w}x{h} produced no output");
                }
            }
        }
    }
}
