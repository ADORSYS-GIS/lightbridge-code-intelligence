//! Pure rendering: given an [`App`], draw the current frame. No I/O, no state mutation.
//!
//! The look is k9s- and opencode-inspired: a fixed header (logo + context block + keymenu), a
//! pill-tab bar, a bordered content table with semantic status coloring and an accent selection
//! cursor, and a status/footer bar with a live spinner + toast. Every color comes from the active
//! [`Theme`] — there are no hardcoded `Color::`s here. The governing discipline: accent only for
//! interactive/selected elements, status in semantic colors, metadata muted, most text in the
//! default foreground.

use super::app::{App, ToastKind, View};
use crate::api::TaskRow;
use crate::theme::{status_label, ButtonKind, Theme};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, TableState,
};
use ratatui::Frame;
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

    let lines = vec![
        entry("a", "approve", a),
        entry("d", "deny", d),
        entry("c", "cancel", c),
        entry("t", "theme", true),
        entry("?", "help", true),
    ];
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
        app.view == View::Runs,
    ));
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

    // LEFT text: " ⠹ filter: pending" (spinner only while loading).
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
        View::Runs => "j/k move · f active/all · r refresh · q quit ",
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
    let area = centered_rect(64, 72, f.area());
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
        row("r", "refresh now"),
        row("f", "cycle filter"),
        Line::from(""),
        section("Actions"),
        row("a", "approve the selected repository"),
        row("d", "deny the selected repository (purges its index)"),
        row("c", "cancel the selected run"),
        Line::from(""),
        section("Appearance & misc"),
        row("t", "cycle the color theme"),
        row("?", "toggle this help"),
        row("q / Esc", "quit"),
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
        use crate::render::{render_to_string, Screen};
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
        use crate::render::{render_to_string, Screen};
        let screens = [
            Screen::Repos,
            Screen::Runs,
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
