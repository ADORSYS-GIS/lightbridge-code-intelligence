//! Modal overlays drawn on top of the current view: the approve/deny/cancel confirm dialog, and the
//! full-screen help/keybinding reference.

use super::super::app::Confirm;
use super::helpers::{centered_rect, pad_left, pad_right};
use crate::theme::{ButtonKind, Theme};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

/// The centered confirm dialog with two button-styled choices.
pub(super) fn draw_confirm(f: &mut Frame, confirm: &Confirm, theme: &Theme) {
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
pub(super) fn draw_help(f: &mut Frame, theme: &Theme) {
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
