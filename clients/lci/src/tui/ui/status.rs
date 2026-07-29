//! The full-width status bar: LEFT filter + spinner, CENTER toast, RIGHT key hint.
//!
//! The three segments are sized to their content — the side segments take exactly what they need
//! (clamped so they never starve each other), and the center toast takes the remainder — instead of
//! rigid percentages that hard-cut a longer hint/toast at 80 cols (gemini). Any segment that still
//! can't fit is truncated with an ellipsis rather than clipped mid-glyph.

use super::super::app::{App, ToastKind, View};
use super::SPINNER;
use super::helpers::{display_width, truncate_ellipsis};
use crate::theme::Theme;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

pub(super) fn draw_status(f: &mut Frame, area: Rect, app: &App, theme: &Theme) {
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
            let live = match app.detail.as_ref() {
                Some(d) if d.live => "live",
                _ => "done",
            };
            format!("{mouse} · {live}")
        }
        View::RepoSettings => match app.repo_settings.as_ref() {
            Some(s) if s.saving => "saving…".to_string(),
            Some(s) if s.loaded => "loaded".to_string(),
            _ => "loading…".to_string(),
        },
    };
    let spinner = if app.loading {
        format!("{} ", SPINNER[app.spinner_frame % SPINNER.len()])
    } else {
        String::new()
    };
    let left_text = format!(" {spinner}{filter}");

    // RIGHT text: the key hint (trailing space keeps it off the edge).
    let hint = match app.view {
        View::Repositories => "j/k move · s settings · f filter · r refresh · q quit ",
        View::Runs => "↵ open · j/k move · f active/all · r refresh · q quit ",
        View::Detail => "m mouse · r refresh · Esc back ",
        View::RepoSettings => "type · ↵ save · Esc back ",
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

#[cfg(test)]
mod tests {
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
}
