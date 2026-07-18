//! The Model-Update half of the TUI's Elm-ish architecture (the View lives in `tui::ui`):
//!
//! - [`state`] — the `App` Model: plain data (which view is active, the loaded rows, selection, auth
//!   status, redraw flags) plus its constructor and read-only queries. No I/O, no key/mouse handling.
//! - [`update`] — a second `impl App` block holding the **transitions**: screen/filter switches,
//!   selection navigation, the confirm-dialog flow, overlay toggles, and the run-detail page's
//!   open/close. Turns an operator intent into a new `App` state.
//! - [`detail`] — the Run Detail page's own sub-model (`DetailState`): task + review data, self-
//!   contained because it's a page with its own life cycle (run observability is Loki-only — epic #459).
//! - [`types`] — the small value types both layers share (`View`, `RepoFilter`, `Toast`, `Confirm`,
//!   `PendingAction`).
//!
//! The raw key/mouse decoding that *drives* these transitions (crossterm `KeyCode` → a call into
//! `update`) lives one level up, in `tui::update` — kept out of here so this module stays terminal-
//! and I/O-free and fully unit-testable.

mod detail;
mod state;
mod types;
mod update;

pub use detail::DetailState;
pub use state::App;
pub use types::{Confirm, PendingAction, ToastKind, View};

#[cfg(test)]
mod tests {
    use super::types::RepoFilter;
    use super::*;
    use crate::api::Claims;
    use crate::api::{Me, RepositoryRow, TaskRow};
    use crate::theme::ButtonKind;
    use std::time::{Duration, Instant};

    fn me_with(perms: &[&str]) -> Me {
        Me {
            claims: Claims {
                sub: "s".into(),
                email: None,
                preferred_username: Some("op".into()),
                name: None,
                exp: Some(0),
            },
            permissions: perms.iter().map(|p| p.to_string()).collect(),
        }
    }

    fn repo(id: i64, status: &str) -> RepositoryRow {
        RepositoryRow {
            id,
            github_repo_id: id,
            owner: "o".into(),
            name: format!("r{id}"),
            default_branch: "main".into(),
            status: status.into(),
            active: status == "approved",
            approved_at: None,
            approved_by: None,
            task_count: 0,
            last_task_at: None,
        }
    }

    fn app() -> App {
        App::new(
            me_with(&["repo:approve", "task:read"]),
            "api.test".into(),
            0,
            Some("rt-0".into()),
            crate::theme::ThemeKind::Midnight,
        )
    }

    #[test]
    fn capability_gates_reflect_permissions() {
        let a = app();
        assert!(a.can_approve());
        assert!(!a.can_deny());
        assert!(!a.can_cancel());
    }

    #[test]
    fn filter_cycles_and_maps_to_query() {
        assert_eq!(RepoFilter::All.next(), RepoFilter::Pending);
        assert_eq!(RepoFilter::Disabled.next(), RepoFilter::All);
        assert_eq!(RepoFilter::Pending.as_query(), Some("pending"));
        assert_eq!(RepoFilter::All.as_query(), None);
    }

    #[test]
    fn navigation_clamps_within_bounds() {
        let mut a = app();
        a.set_repos(vec![repo(1, "pending"), repo(2, "approved")]);
        assert_eq!(a.repo_selected, 0);
        a.select_prev(); // clamps at 0
        assert_eq!(a.repo_selected, 0);
        a.select_next();
        assert_eq!(a.repo_selected, 1);
        a.select_next(); // clamps at last
        assert_eq!(a.repo_selected, 1);
        assert_eq!(a.selected_repo().unwrap().id, 2);
    }

    #[test]
    fn shrinking_data_reclamps_selection() {
        let mut a = app();
        a.set_repos(vec![
            repo(1, "pending"),
            repo(2, "pending"),
            repo(3, "pending"),
        ]);
        a.select_next();
        a.select_next();
        assert_eq!(a.repo_selected, 2);
        a.set_repos(vec![repo(1, "pending")]); // shrink
        assert_eq!(a.repo_selected, 0, "selection reclamped after data shrank");
    }

    fn ask_approve(a: &mut App) {
        a.ask_confirm(
            "Approve o/r5?",
            "opens the gate",
            "Approve",
            ButtonKind::Primary,
            PendingAction::Approve(5),
        );
    }

    #[test]
    fn confirm_returns_action_only_when_accepted() {
        let mut a = app();
        ask_approve(&mut a);
        assert!(a.confirm.is_some());
        let action = a.resolve_confirm(false);
        assert!(action.is_none(), "declined confirm yields no action");
        assert!(a.confirm.is_none(), "prompt cleared on decline");

        ask_approve(&mut a);
        match a.resolve_confirm(true) {
            Some(PendingAction::Approve(5)) => {}
            other => panic!("expected Approve(5), got {other:?}"),
        }
    }

    #[test]
    fn confirm_defaults_to_cancel_and_enter_respects_focus() {
        let mut a = app();
        ask_approve(&mut a);
        // Focus defaults to the safe Cancel button: a reflexive Enter must NOT act.
        assert!(
            !a.confirm.as_ref().unwrap().confirm_focused,
            "focus starts on Cancel"
        );
        assert!(
            a.resolve_confirm_focused().is_none(),
            "Enter on Cancel declines"
        );

        // Move focus to the affirmative button, then Enter acts.
        ask_approve(&mut a);
        a.confirm_toggle_focus();
        assert!(a.confirm.as_ref().unwrap().confirm_focused);
        match a.resolve_confirm_focused() {
            Some(PendingAction::Approve(5)) => {}
            other => panic!("expected Approve(5) after focusing the button, got {other:?}"),
        }
    }

    #[test]
    fn cycle_theme_advances_and_wraps() {
        let mut a = app();
        assert_eq!(a.theme_kind, crate::theme::ThemeKind::Midnight);
        a.cycle_theme();
        assert_eq!(a.theme_kind, crate::theme::ThemeKind::Terminal);
        a.cycle_theme();
        a.cycle_theme();
        assert_eq!(
            a.theme_kind,
            crate::theme::ThemeKind::Midnight,
            "wraps back"
        );
    }

    #[test]
    fn toast_expires_after_ttl() {
        let mut a = app();
        a.toast_success("done");
        assert!(a.toast.is_some());
        // Not yet expired.
        assert!(!a.tick_toast(Instant::now()));
        // Simulate elapsed time by backdating shown_at.
        if let Some(t) = &mut a.toast {
            t.shown_at = Instant::now() - Duration::from_secs(10);
        }
        assert!(a.tick_toast(Instant::now()));
        assert!(a.toast.is_none());
    }

    fn task_row(status: &str) -> TaskRow {
        TaskRow {
            id: uuid::Uuid::from_u128(42),
            repository_id: 1,
            target_type: "pull_request".into(),
            target_id: 128,
            command_text: "review".into(),
            kind: "review".into(),
            status: status.into(),
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            started_at: None,
            completed_at: None,
            repo_owner: Some("vymalo".into()),
            repo_name: Some("lci".into()),
            job_name: Some("review-abc".into()),
            error_detail: None,
            base_sha: Some("a1b2c3d4e5f6".into()),
            head_sha: Some("e4f5a6b7c8d9".into()),
        }
    }

    #[test]
    fn open_detail_only_from_runs_with_a_selection() {
        let mut a = app();
        a.set_view(View::Runs);
        a.set_tasks(vec![task_row("running")]);
        a.open_detail();
        assert_eq!(a.view, View::Detail);
        let d = a.detail.as_ref().expect("detail open");
        assert!(d.live, "a running task reads as live");
        a.close_detail();
        assert_eq!(a.view, View::Runs);
        assert!(a.detail.is_none());
    }

    #[test]
    fn detail_permission_denied_when_missing_review_read() {
        // `app()` grants only repo:approve + task:read → no review:read.
        let mut a = app();
        a.set_view(View::Runs);
        a.set_tasks(vec![task_row("running")]);
        a.open_detail();
        let d = a.detail.as_ref().unwrap();
        assert!(
            d.permission_denied,
            "no review:read → inline notice, no fetch"
        );
    }

    #[test]
    fn toggle_mouse_flips_state() {
        let mut a = app();
        assert!(a.mouse_enabled, "capture on by default");
        assert!(!a.toggle_mouse());
        assert!(a.toggle_mouse());
    }

    #[test]
    fn runs_active_filter_hides_terminal_tasks() {
        let mut a = app();
        a.set_view(View::Runs);
        let base = TaskRow {
            id: uuid::Uuid::new_v4(),
            repository_id: 1,
            target_type: "pull_request".into(),
            target_id: 1,
            command_text: "review".into(),
            kind: "review".into(),
            status: "running".into(),
            created_at: time::OffsetDateTime::UNIX_EPOCH,
            started_at: None,
            completed_at: None,
            repo_owner: None,
            repo_name: None,
            job_name: None,
            error_detail: None,
            base_sha: None,
            head_sha: None,
        };
        let done = TaskRow {
            status: "succeeded".into(),
            ..base.clone()
        };
        a.set_tasks(vec![base, done]);
        assert_eq!(a.visible_tasks().len(), 1, "only the active task shows");
        a.cycle_filter(); // active_only -> false
        assert_eq!(a.visible_tasks().len(), 2);
    }
}
