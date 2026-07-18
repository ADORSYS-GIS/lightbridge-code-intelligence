//! Pure rendering: given an [`App`], draw the current frame. No I/O, no state mutation.
//!
//! The look is k9s- and opencode-inspired: a fixed header (logo + context block + keymenu), a
//! pill-tab bar, a bordered content table with semantic status coloring and an accent selection
//! cursor, and a status/footer bar with a live spinner + toast. Every color comes from the active
//! [`Theme`] — there are no hardcoded `Color::`s here. The governing discipline: accent only for
//! interactive/selected elements, status in semantic colors, metadata muted, most text in the
//! default foreground.
//!
//! Split by screen/panel (each a thin "sub-view" of the whole frame): [`header`] + [`tabs`] draw the
//! chrome; [`repositories`] / [`runs`] / [`detail`] draw the three content views; [`status`] draws the
//! footer bar; [`overlays`] draws the confirm dialog + help screen; [`helpers`] holds the small
//! formatting/layout utilities shared across them.

mod detail;
mod header;
mod helpers;
mod overlays;
mod repositories;
mod runs;
mod status;
mod tabs;

use super::app::{App, View};
use crate::theme::Theme;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

/// The braille spinner cycle (8 frames), width-1 each. Shared by [`status`]'s footer spinner.
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

    header::draw_header(f, chunks[0], app, &theme);
    tabs::draw_tabs(f, chunks[1], app, &theme);
    match app.view {
        View::Repositories => repositories::draw_repositories(f, chunks[2], app, &theme),
        View::Runs => runs::draw_runs(f, chunks[2], app, &theme),
        View::Detail => detail::draw_detail(f, chunks[2], app, &theme),
    }
    status::draw_status(f, chunks[3], app, &theme);

    if let Some(confirm) = &app.confirm {
        overlays::draw_confirm(f, confirm, &theme);
    }
    if app.show_help {
        overlays::draw_help(f, &theme);
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
    .alignment(ratatui::layout::Alignment::Center);
    f.render_widget(msg, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Claims, Me, TaskRow};
    use crate::tui::app::View as AppView;
    use time::OffsetDateTime;

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

    /// Draw an [`App`] in the detail view to a plain-text buffer (styling dropped), for asserting the
    /// meta/error layout without the `--render` seed screens. Uses the real `open_detail` path.
    fn draw_detail_to_string(task: TaskRow, w: u16, h: u16) -> String {
        use crate::theme::ThemeKind;
        use crate::tui::app::App;
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
        app.set_view(AppView::Runs);
        app.runs_active_only = false; // so a terminal (failed) task is visible + selectable
        app.set_tasks(vec![task]);
        app.open_detail();
        assert_eq!(app.view, AppView::Detail, "detail opened via the real path");
        if let Some(d) = app.detail.as_mut() {
            d.review_loaded = true;
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
    fn draws_every_screen_at_many_sizes_without_panicking() {
        use crate::render::{Screen, render_to_string};
        use crate::theme::ThemeKind;
        let screens = [
            Screen::Repos,
            Screen::Runs,
            Screen::Detail,
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
