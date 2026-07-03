//! A headless renderer: draw any TUI screen to a plain-text buffer via ratatui's `TestBackend`, with
//! no real terminal, no auth, and no network. It seeds realistic fake data so the layout is
//! reviewable in a diff or a PR body, and it powers the `lci --render <screen>` dev affordance and the
//! snapshot tests.
//!
//! This lives outside `tui` (which owns the crossterm-backed terminal) precisely so it can drive the
//! same pure [`crate::tui::ui::draw`] against a test backend.

use crate::api::{Claims, Me, RepositoryRow, TaskRow};
use crate::theme::{ButtonKind, ThemeKind};
use crate::tui::app::{App, PendingAction};
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use time::OffsetDateTime;

/// The screens the renderer can draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// Repositories list with pending/approved/disabled rows.
    Repos,
    /// Runs list with a running + failed row.
    Runs,
    /// The approve confirm dialog (over the repos list).
    Confirm,
    /// The help overlay.
    Help,
    /// The empty state (no repositories under the filter).
    Empty,
    /// The too-small terminal fallback.
    TooSmall,
}

impl Screen {
    /// Parse a screen name from the `--render` argument.
    pub fn from_name(s: &str) -> Option<Screen> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "repos" | "repositories" => Screen::Repos,
            "runs" => Screen::Runs,
            "confirm" => Screen::Confirm,
            "help" => Screen::Help,
            "empty" => Screen::Empty,
            "too-small" | "toosmall" | "small" => Screen::TooSmall,
            _ => return None,
        })
    }

    /// The accepted names, for an error message.
    pub const NAMES: &'static str = "repos | runs | confirm | help | empty | too-small";
}

/// Build a seeded [`App`] for a screen, with the given theme.
fn seeded_app(screen: Screen, theme: ThemeKind) -> App {
    let me = Me {
        claims: Claims {
            sub: "op-1".into(),
            email: Some("op@example.test".into()),
            preferred_username: Some("operator".into()),
            name: Some("The Operator".into()),
            exp: Some(0),
        },
        permissions: vec![
            "repo:read".into(),
            "repo:approve".into(),
            "repo:deny".into(),
            "task:read".into(),
            "task:cancel".into(),
        ],
    };
    // A token that reads as "5m00s" in the header, regardless of when the snapshot runs.
    let expires_at = crate::auth::now_unix() + 300;
    let mut app = App::new(
        me,
        "code-intelligence-api.ai.camer.digital".into(),
        expires_at,
        Some("rt-seed".into()),
        theme,
    );

    match screen {
        Screen::Repos | Screen::Confirm | Screen::TooSmall => app.set_repos(sample_repos()),
        Screen::Runs => {
            app.set_view(crate::tui::app::View::Runs);
            app.runs_active_only = false; // show the failed row too
            app.set_tasks(sample_tasks());
        }
        Screen::Empty => {
            // A pending filter over an empty list.
            app.set_repos(Vec::new());
        }
        Screen::Help => {
            app.set_repos(sample_repos());
            app.show_help = true;
        }
    }

    if screen == Screen::Confirm {
        app.ask_confirm(
            "Approve vymalo/lightbridge-code-intelligence?",
            "Opens the gate and triggers indexing.",
            "Approve",
            ButtonKind::Primary,
            PendingAction::Approve(7),
        );
        // Focus the affirmative button so the snapshot shows the "gamified" highlighted state.
        app.confirm_toggle_focus();
    }

    app
}

/// A fixed timestamp so snapshots are deterministic.
fn fixed_ts() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_751_450_130).unwrap() // 2025-07-02T10:15:30Z
}

fn sample_repos() -> Vec<RepositoryRow> {
    let mk = |id: i64, owner: &str, name: &str, status: &str, tasks: i64, by: Option<&str>| {
        RepositoryRow {
            id,
            github_repo_id: 900_000 + id,
            owner: owner.into(),
            name: name.into(),
            default_branch: "main".into(),
            status: status.into(),
            active: status == "approved",
            approved_at: (status == "approved").then(fixed_ts),
            approved_by: by.map(String::from),
            task_count: tasks,
            last_task_at: (tasks > 0).then(fixed_ts),
        }
    };
    vec![
        mk(
            7,
            "vymalo",
            "lightbridge-code-intelligence",
            "pending",
            12,
            None,
        ),
        mk(8, "vymalo", "ai-helm", "approved", 48, Some("operator")),
        mk(9, "adorsys-gis", "ai-governance", "pending", 0, None),
        mk(10, "vymalo", "home-os", "disabled", 3, Some("operator")),
        mk(11, "vymalo", "eaig", "approved", 21, Some("alice")),
    ]
}

fn sample_tasks() -> Vec<TaskRow> {
    let mk = |id_seed: u128,
              status: &str,
              owner: &str,
              name: &str,
              ttype: &str,
              tid: i64,
              age_secs: i64,
              job: Option<&str>| {
        TaskRow {
            id: uuid::Uuid::from_u128(id_seed),
            repository_id: 7,
            target_type: ttype.into(),
            target_id: tid,
            command_text: "review".into(),
            kind: "review".into(),
            status: status.into(),
            created_at: OffsetDateTime::from_unix_timestamp(crate::auth::now_unix() - age_secs)
                .unwrap_or(OffsetDateTime::UNIX_EPOCH),
            started_at: None,
            completed_at: None,
            repo_owner: Some(owner.into()),
            repo_name: Some(name.into()),
            job_name: job.map(String::from),
            error_detail: None,
        }
    };
    vec![
        mk(
            1,
            "running",
            "vymalo",
            "lightbridge-code-intelligence",
            "pull_request",
            128,
            95,
            Some("review-9f2a"),
        ),
        mk(
            2,
            "queued",
            "vymalo",
            "ai-helm",
            "pull_request",
            44,
            20,
            None,
        ),
        mk(
            3,
            "waiting_for_index",
            "adorsys-gis",
            "ai-governance",
            "issue",
            12,
            8,
            None,
        ),
        mk(
            4,
            "succeeded",
            "vymalo",
            "eaig",
            "pull_request",
            301,
            3600,
            Some("review-77c1"),
        ),
        mk(
            5,
            "failed",
            "vymalo",
            "home-os",
            "pull_request",
            9,
            7200,
            Some("review-4d0e"),
        ),
    ]
}

/// Render a screen to a plain-text string (one line per buffer row, trailing blanks trimmed). Each
/// cell contributes its symbol; styling is dropped (this is a layout snapshot, not a color one).
pub fn render_to_string(screen: Screen, width: u16, height: u16, theme: ThemeKind) -> String {
    let app = seeded_app(screen, theme);
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|f| crate::tui::ui::draw(f, &app))
        .expect("draw");

    let buffer = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..height {
        let mut line = String::new();
        for x in 0..width {
            line.push_str(buffer[(x, y)].symbol());
        }
        // Trim trailing spaces so the snapshot isn't a wall of padding.
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// The `lci --render <screen>` entrypoint: render the requested screen and print it to stdout.
pub fn run(spec: &crate::cli::RenderSpec) -> anyhow::Result<()> {
    let screen = Screen::from_name(&spec.screen).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown --render screen `{}` (expected one of: {})",
            spec.screen,
            Screen::NAMES
        )
    })?;
    let theme = ThemeKind::from_name(&spec.theme);
    print!(
        "{}",
        render_to_string(screen, spec.width, spec.height, theme)
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screen_names_round_trip() {
        assert_eq!(Screen::from_name("repos"), Some(Screen::Repos));
        assert_eq!(Screen::from_name("TOO-SMALL"), Some(Screen::TooSmall));
        assert_eq!(Screen::from_name("nope"), None);
    }

    #[test]
    fn repos_snapshot_has_logo_tabs_and_a_status() {
        // At 80 cols the REPOSITORY column truncates long names, so assert on a prefix here.
        let narrow = render_to_string(Screen::Repos, 80, 24, ThemeKind::Midnight);
        assert!(narrow.contains("LCI"), "logo present");
        assert!(narrow.contains("Repositories"), "tab present");
        assert!(
            narrow.contains("vymalo/lightbridge-code"),
            "a repo row present"
        );
        assert!(narrow.contains("pending"), "a status word present");
        assert!(narrow.contains("operator"), "identity present");

        // At 120 the full name fits.
        let wide = render_to_string(Screen::Repos, 120, 40, ThemeKind::Midnight);
        assert!(
            wide.contains("vymalo/lightbridge-code-intelligence"),
            "full repo name at 120 cols"
        );
    }

    #[test]
    fn confirm_snapshot_shows_both_buttons() {
        let s = render_to_string(Screen::Confirm, 80, 24, ThemeKind::Midnight);
        assert!(s.contains("Confirm"), "dialog title");
        assert!(s.contains("Approve"), "affirmative button");
        assert!(s.contains("Cancel"), "cancel button");
        // The focused affirmative button carries the ›‹ markers.
        assert!(s.contains('›') && s.contains('‹'), "focused-button markers");
    }

    #[test]
    fn help_snapshot_lists_keys() {
        let s = render_to_string(Screen::Help, 80, 24, ThemeKind::Midnight);
        assert!(s.contains("Help"));
        assert!(s.contains("approve"));
        assert!(s.contains("theme"));
    }

    #[test]
    fn empty_snapshot_is_an_inline_status_line() {
        let s = render_to_string(Screen::Empty, 80, 24, ThemeKind::Midnight);
        assert!(s.contains("no pending repositories") || s.contains("no pending"));
    }

    #[test]
    fn runs_snapshot_shows_running_and_failed() {
        let s = render_to_string(Screen::Runs, 120, 40, ThemeKind::Midnight);
        assert!(s.contains("running"));
        assert!(s.contains("failed"));
        assert!(s.contains("PR #128"));
    }

    #[test]
    fn too_small_snapshot_is_graceful() {
        let s = render_to_string(Screen::TooSmall, 40, 10, ThemeKind::Midnight);
        assert!(
            s.contains("too small"),
            "graceful message, not a clipped mess"
        );
    }

    #[test]
    fn terminal_theme_renders_without_panicking() {
        // The transparent-background theme must lay out identically.
        let s = render_to_string(Screen::Repos, 80, 24, ThemeKind::Terminal);
        assert!(s.contains("Repositories"));
    }
}
