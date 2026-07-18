//! A headless renderer: draw any TUI screen to a plain-text buffer via ratatui's `TestBackend`, with
//! no real terminal, no auth, and no network. It seeds realistic fake data so the layout is
//! reviewable in a diff or a PR body, and it powers the `lci --render <screen>` dev affordance and the
//! snapshot tests.
//!
//! This lives outside `tui` (which owns the crossterm-backed terminal) precisely so it can drive the
//! same pure [`crate::tui::ui::draw`] against a test backend.

use crate::api::RepositoryRow;
use crate::api::{Claims, Me, ReviewRow, TaskRow};
use crate::theme::{ButtonKind, ThemeKind};
use crate::tui::app::{App, DetailState, PendingAction, View};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use time::OffsetDateTime;

/// The screens the renderer can draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// Repositories list with pending/approved/disabled rows.
    Repos,
    /// Runs list with a running + failed row.
    Runs,
    /// The Run Detail page: meta + review, on a *terminal* (done) run with a review.
    Detail,
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
            "detail" => Screen::Detail,
            "confirm" => Screen::Confirm,
            "help" => Screen::Help,
            "empty" => Screen::Empty,
            "too-small" | "toosmall" | "small" => Screen::TooSmall,
            _ => return None,
        })
    }

    /// The accepted names, comma-separated, for the list output + error message.
    pub const NAMES: &'static str = "repos, runs, detail, confirm, help, empty, small";
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
        // A completed run WITH a review (the reviewable "static" detail state).
        Screen::Detail => {
            app.set_view(View::Runs);
            app.runs_active_only = false;
            app.set_tasks(sample_tasks());
            let mut d = DetailState::new(sample_detail_task("succeeded", 3600), true);
            d.review = Some(sample_review());
            d.review_loaded = true;
            app.detail = Some(d);
            app.view = View::Detail;
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
    OffsetDateTime::from_unix_timestamp(1_782_986_130).unwrap() // 2026-07-02T09:55:30Z
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
            base_sha: None,
            head_sha: None,
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

/// A single detailed task for the Run Detail snapshots. `age_secs` back-dates `created_at`; a
/// terminal status also gets a `completed_at` so the duration renders.
fn sample_detail_task(status: &str, age_secs: i64) -> TaskRow {
    let created = crate::auth::now_unix() - age_secs;
    let started = created + 5;
    let terminal = !matches!(
        status,
        "received" | "waiting_for_index" | "queued" | "running" | "posting_result"
    );
    TaskRow {
        id: uuid::Uuid::from_u128(0x3f2504e04f8941d39a0c0305e82c3301),
        repository_id: 7,
        target_type: "pull_request".into(),
        target_id: 128,
        command_text: "review".into(),
        kind: "review".into(),
        status: status.into(),
        created_at: OffsetDateTime::from_unix_timestamp(created)
            .unwrap_or(OffsetDateTime::UNIX_EPOCH),
        started_at: OffsetDateTime::from_unix_timestamp(started).ok(),
        completed_at: terminal
            .then(|| OffsetDateTime::from_unix_timestamp(created + age_secs).ok())
            .flatten(),
        repo_owner: Some("vymalo".into()),
        repo_name: Some("lightbridge-code-intelligence".into()),
        job_name: Some("review-9f2a".into()),
        error_detail: matches!(status, "failed" | "timed_out")
            .then(|| "agent runner exited 137 (OOM-killed) after 2 tool calls".to_string()),
        base_sha: Some("a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4".into()),
        head_sha: Some("e4f5a6b7c8d90e1f2a3b4c5d6e7f8091a2b3c4d5".into()),
    }
}

fn sample_review() -> ReviewRow {
    ReviewRow {
        task_id: uuid::Uuid::from_u128(0x3f2504e04f8941d39a0c0305e82c3301),
        summary: "Solid change; two inline nits and one deferred concern about retry backoff."
            .into(),
        body: "Review body (markdown) omitted in the TUI.".into(),
        inline_count: 2,
        deferred_count: 1,
        out_of_scope_count: 0,
        findings: serde_json::json!({"inline": [{"path": "src/main.rs", "line": 42}]}),
        review_url: Some(
            "https://github.com/vymalo/lightbridge-code-intelligence/pull/128#pullrequestreview-1"
                .into(),
        ),
        github_review_id: Some(987654),
        created_at: fixed_ts(),
    }
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

/// The `lci --render [screen]` entrypoint. `list` (or no name) prints the valid screen names and
/// exits 0; a known name renders that screen; an unknown name errors with the same valid list.
pub fn run(spec: &crate::cli::RenderSpec) -> anyhow::Result<()> {
    // `--render` alone / `--render list` → print the menu and exit successfully.
    if matches!(
        spec.screen.trim().to_ascii_lowercase().as_str(),
        "list" | ""
    ) {
        println!("valid --render screens: {}", Screen::NAMES);
        println!("example: lci --render detail --width 120 --theme nord");
        return Ok(());
    }
    let screen = Screen::from_name(&spec.screen).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown --render screen `{}`. valid: {}. \
             example: lci --render detail --width 120 --theme nord",
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
        assert_eq!(Screen::from_name("detail"), Some(Screen::Detail));
        assert_eq!(Screen::from_name("TOO-SMALL"), Some(Screen::TooSmall));
        assert_eq!(Screen::from_name("nope"), None);
        // `list` is handled by `run`, not a drawable screen.
        assert_eq!(Screen::from_name("list"), None);
    }

    #[test]
    fn detail_snapshot_has_meta_and_review() {
        let s = render_to_string(Screen::Detail, 80, 24, ThemeKind::Midnight);
        // Meta panel.
        assert!(s.contains("Run "), "meta panel title");
        assert!(s.contains("PR #128"), "target");
        assert!(s.contains("done"), "status short label for succeeded");
        assert!(s.contains("● done"), "terminal live badge");
        // The base→head short SHAs render (7-char each) — not the old `—→—` placeholder.
        assert!(s.contains("a1b2c3d→e4f5a6b"), "short base→head SHAs:\n{s}");
        assert!(
            !s.contains("—→—"),
            "no SHA placeholder now the fields exist"
        );
        // Review panel — now the bottom panel filling the page (no transcript panel post-#459).
        assert!(s.contains("Review"), "review panel");
        assert!(s.contains("inline"), "finding tally");
        assert!(!s.contains("Transcript"), "transcript panel is gone (#459)");

        // The 120x40 size must also render (used in the PR body); the wrapped summary shows.
        let wide = render_to_string(Screen::Detail, 120, 40, ThemeKind::Midnight);
        assert!(
            wide.contains("retry backoff") || wide.contains("backoff"),
            "review summary wrapped in"
        );
    }

    #[test]
    fn detail_degrades_on_a_small_terminal_without_panicking() {
        // Below the min size it must show the guard, not a clipped mess or a panic.
        let s = render_to_string(Screen::Detail, 40, 10, ThemeKind::Midnight);
        assert!(s.contains("too small"));
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
        // The STATUS column uses short labels — no mid-word truncation of the long ones.
        assert!(s.contains("indexing"), "waiting_for_index → indexing");
        assert!(s.contains("done"), "succeeded → done");
        assert!(
            !s.contains("waiting_for_inde"),
            "no mid-word cut of waiting_for_index"
        );
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
    fn render_run_lists_on_no_name_and_errors_on_unknown() {
        use crate::cli::RenderSpec;
        // `list` (the no-value default) succeeds and prints — must not error.
        let list = RenderSpec {
            screen: "list".into(),
            ..RenderSpec::default()
        };
        assert!(run(&list).is_ok(), "list exits 0");

        // An unknown screen errors, and the error names the valid list (so it's actionable).
        let bad = RenderSpec {
            screen: "wat".into(),
            ..RenderSpec::default()
        };
        let err = run(&bad).unwrap_err().to_string();
        assert!(err.contains("wat"), "echoes the bad name");
        assert!(
            err.contains("detail") && err.contains("runs"),
            "lists valid names"
        );
        assert!(err.contains("valid:"), "uses the valid: phrasing");
    }

    #[test]
    fn names_list_includes_the_detail_screen() {
        assert!(Screen::NAMES.contains("detail"));
    }

    #[test]
    fn terminal_theme_renders_without_panicking() {
        // The transparent-background theme must lay out identically.
        let s = render_to_string(Screen::Repos, 80, 24, ThemeKind::Terminal);
        assert!(s.contains("Repositories"));
    }
}
