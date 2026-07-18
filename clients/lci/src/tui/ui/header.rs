//! The header row: a `▍ LCI` logo, a k9s-style `key: value` context block (host/user/perms/token),
//! and a right-aligned keymenu — all on the theme's surface fill.

use super::super::app::{App, View};
use super::helpers::{pad_left, pad_right};
use crate::theme::Theme;
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

/// The header: a `▍ LCI` logo on the left, a k9s-style context block in the center, and a keymenu on
/// the right — all on the surface fill.
pub(super) fn draw_header(f: &mut Frame, area: Rect, app: &App, theme: &Theme) {
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
    let lines = match app.view {
        View::Detail => vec![
            entry("↵/l", "open", true),
            entry("Esc", "back", true),
            entry("m", "mouse", true),
            entry("?", "help", true),
        ],
        _ => vec![
            entry("a", "approve", a),
            entry("d", "deny", d),
            entry("c", "cancel", c),
            entry("m", "mouse", true),
            entry("?", "help", true),
        ],
    };
    f.render_widget(
        Paragraph::new(lines)
            .alignment(Alignment::Right)
            .style(theme.surface_style()),
        pad_right(area, 1),
    );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perm_summary_lists_capabilities() {
        let s = perm_summary(&["repo:approve".into(), "repo:deny".into()]);
        assert!(s.contains("approve"));
        assert!(s.contains("deny"));
        let ro = perm_summary(&["repo:read".into()]);
        assert!(ro.contains("read-only"));
    }
}
