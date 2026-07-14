//! The Run Detail page: a stacked meta panel, a small review panel, and the large live-tailing
//! transcript panel — joined with **manually collapsed borders** (the lower panel drops its top
//! border so it reads as one continuous frame). ratatui 0.30 could express this with `merge_borders`
//! / `Spacing::Overlap`, but 0.29 lacks those, so we omit the touching edge by hand.

use super::super::app::{App, DetailState};
use super::helpers::{fmt_ts, pad_left, repo_label, target_label, truncate_ellipsis};
use crate::api::TaskRow;
use crate::theme::{Theme, status_label};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};
use unicode_width::UnicodeWidthChar;

/// The Run Detail page: a stacked meta panel, a small review panel, and the large live-tailing
/// transcript panel.
pub(super) fn draw_detail(f: &mut Frame, area: Rect, app: &App, theme: &Theme) {
    let Some(d) = app.detail.as_ref() else {
        return;
    };

    // Meta 8 rows (6 inner k/v rows: the right column's sha/created/started/completed/duration/job).
    // On a failed/timed-out run we add a row (→9) so the error-detail line has its own row instead of
    // overwriting `job`. Review 4 rows; transcript takes the rest. `Min(3)` guards a graceful degrade
    // on short terminals — at the 80×24 review size the non-error case sums exactly to the content area.
    let meta_height = match d.task.status.as_str() {
        "failed" | "timed_out" => 9,
        _ => 8,
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
    match (d.new_since_scroll > 0, d.live && d.autoscroll) {
        (true, _) => {
            title_spans.push(Span::raw(" "));
            title_spans.push(Span::styled(
                format!("▼ {} new", d.new_since_scroll),
                Style::default().fg(theme.info).add_modifier(Modifier::BOLD),
            ));
        }
        (false, true) => {
            title_spans.push(Span::raw(" "));
            title_spans.push(Span::styled("tailing", theme.muted_text()));
        }
        (false, false) => {}
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
        let hint = match (d.transcript_loaded, d.live) {
            (false, _) => "• loading transcript…",
            (true, true) => "• no turns yet — the agent hasn't logged activity (tailing…)",
            (true, false) => "• no transcript recorded for this run",
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
