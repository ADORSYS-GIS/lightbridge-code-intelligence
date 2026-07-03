//! The application state machine: which view is active, the loaded rows, selection, filters, the
//! toast/confirm/help overlays, and the auth status. Kept free of I/O and rendering so the state
//! transitions are unit-testable; the event loop (`tui::run`) drives it and the renderer (`tui::ui`)
//! reads it.

use crate::api::{Me, RepositoryRow, TaskRow};
use std::time::{Duration, Instant};

/// The two operator views.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Repositories,
    Runs,
}

/// The status filter cycled with `f` on the Repositories view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoFilter {
    All,
    Pending,
    Approved,
    Disabled,
}

impl RepoFilter {
    /// The `?status=` query value (`None` for `All`).
    pub fn as_query(self) -> Option<&'static str> {
        match self {
            RepoFilter::All => None,
            RepoFilter::Pending => Some("pending"),
            RepoFilter::Approved => Some("approved"),
            RepoFilter::Disabled => Some("disabled"),
        }
    }

    /// A short label for the status bar.
    pub fn label(self) -> &'static str {
        match self {
            RepoFilter::All => "all",
            RepoFilter::Pending => "pending",
            RepoFilter::Approved => "approved",
            RepoFilter::Disabled => "disabled",
        }
    }

    /// Cycle to the next filter (wraps).
    pub fn next(self) -> Self {
        match self {
            RepoFilter::All => RepoFilter::Pending,
            RepoFilter::Pending => RepoFilter::Approved,
            RepoFilter::Approved => RepoFilter::Disabled,
            RepoFilter::Disabled => RepoFilter::All,
        }
    }
}

/// A transient status message shown at the bottom for a few seconds.
#[derive(Debug, Clone)]
pub struct Toast {
    pub text: String,
    pub kind: ToastKind,
    pub shown_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Error,
}

/// A pending confirmation prompt (`a`/`d`/`c` ask before acting).
#[derive(Debug, Clone)]
pub struct Confirm {
    pub prompt: String,
    pub action: PendingAction,
}

/// The action a confirmation will trigger once accepted.
#[derive(Debug, Clone)]
pub enum PendingAction {
    Approve(i64),
    Deny(i64),
    Cancel(uuid::Uuid),
}

/// The whole app state.
pub struct App {
    pub view: View,
    pub me: Option<Me>,
    pub api_host: String,

    pub repos: Vec<RepositoryRow>,
    pub repo_filter: RepoFilter,
    pub repo_selected: usize,

    pub tasks: Vec<TaskRow>,
    pub runs_active_only: bool,
    pub run_selected: usize,

    pub toast: Option<Toast>,
    pub confirm: Option<Confirm>,
    pub show_help: bool,

    /// Absolute token expiry (unix seconds) for the countdown; `None` until known.
    pub token_expires_at: Option<i64>,
    /// Set when background refresh fails — surfaced in the status bar.
    pub reauth_needed: bool,
    /// The session's current refresh token, **rotated** on every successful refresh (Keycloak issues
    /// a new one and revokes the old). `None` once we have no usable refresh token.
    pub refresh_token: Option<String>,
    /// Latch set after a fatal refresh failure (e.g. `invalid_grant`) so we stop re-firing a dead
    /// refresh token against the IdP every interval (P1: that hot-looped). Cleared only by re-login.
    pub refresh_disabled: bool,

    pub should_quit: bool,
    /// Set when a mutation (approve/deny/cancel) succeeds so the event loop re-fetches the current
    /// view on the next tick instead of waiting for the periodic refresh (P2).
    needs_view_refresh: bool,
    /// Bumped whenever state changes in a way that needs a redraw.
    dirty: bool,
}

/// How long a toast stays up.
const TOAST_TTL: Duration = Duration::from_secs(4);

impl App {
    pub fn new(
        me: Me,
        api_host: String,
        token_expires_at: i64,
        refresh_token: Option<String>,
    ) -> Self {
        Self {
            view: View::Repositories,
            me: Some(me),
            api_host,
            repos: Vec::new(),
            repo_filter: RepoFilter::Pending,
            repo_selected: 0,
            tasks: Vec::new(),
            runs_active_only: true,
            run_selected: 0,
            toast: None,
            confirm: None,
            show_help: false,
            token_expires_at: Some(token_expires_at),
            reauth_needed: false,
            refresh_token,
            refresh_disabled: false,
            should_quit: false,
            needs_view_refresh: false,
            dirty: true,
        }
    }

    /// Flag that the current view should be re-fetched on the next event-loop tick (after a
    /// successful mutation). Consumed by [`Self::take_view_refresh`].
    pub fn request_view_refresh(&mut self) {
        self.needs_view_refresh = true;
    }

    /// Consume the view-refresh request, returning whether a re-fetch is due.
    pub fn take_view_refresh(&mut self) -> bool {
        std::mem::replace(&mut self.needs_view_refresh, false)
    }

    /// Whether a redraw is pending; clears the flag.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::replace(&mut self.dirty, false)
    }

    /// Force a redraw on the next frame.
    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    // --- capability gates (mirror /me perms) ---
    pub fn can_approve(&self) -> bool {
        self.me.as_ref().is_some_and(|m| m.can("repo:approve"))
    }
    pub fn can_deny(&self) -> bool {
        self.me.as_ref().is_some_and(|m| m.can("repo:deny"))
    }
    pub fn can_cancel(&self) -> bool {
        self.me.as_ref().is_some_and(|m| m.can("task:cancel"))
    }

    // --- data replacement (from async fetches) ---
    pub fn set_repos(&mut self, repos: Vec<RepositoryRow>) {
        self.repos = repos;
        self.clamp_selection();
        self.mark_dirty();
    }
    pub fn set_tasks(&mut self, tasks: Vec<TaskRow>) {
        self.tasks = tasks;
        self.clamp_selection();
        self.mark_dirty();
    }

    /// The tasks currently visible under the active filter.
    pub fn visible_tasks(&self) -> Vec<&TaskRow> {
        self.tasks
            .iter()
            .filter(|t| !self.runs_active_only || t.is_active())
            .collect()
    }

    /// The currently-selected repository, if any.
    pub fn selected_repo(&self) -> Option<&RepositoryRow> {
        self.repos.get(self.repo_selected)
    }

    /// The currently-selected task under the active filter, if any.
    pub fn selected_task(&self) -> Option<&TaskRow> {
        self.visible_tasks().get(self.run_selected).copied()
    }

    // --- navigation ---
    pub fn select_next(&mut self) {
        let len = self.current_len();
        if len == 0 {
            return;
        }
        let sel = self.current_selection();
        self.set_selection((sel + 1).min(len - 1));
        self.mark_dirty();
    }
    pub fn select_prev(&mut self) {
        let sel = self.current_selection();
        self.set_selection(sel.saturating_sub(1));
        self.mark_dirty();
    }

    fn current_len(&self) -> usize {
        match self.view {
            View::Repositories => self.repos.len(),
            View::Runs => self.visible_tasks().len(),
        }
    }
    fn current_selection(&self) -> usize {
        match self.view {
            View::Repositories => self.repo_selected,
            View::Runs => self.run_selected,
        }
    }
    fn set_selection(&mut self, idx: usize) {
        match self.view {
            View::Repositories => self.repo_selected = idx,
            View::Runs => self.run_selected = idx,
        }
    }

    /// Keep selection indices in range after data changes.
    fn clamp_selection(&mut self) {
        let repo_max = self.repos.len().saturating_sub(1);
        self.repo_selected = self.repo_selected.min(repo_max);
        let run_max = self.visible_tasks().len().saturating_sub(1);
        self.run_selected = self.run_selected.min(run_max);
    }

    // --- view + filter switches ---
    pub fn set_view(&mut self, view: View) {
        if self.view != view {
            self.view = view;
            self.mark_dirty();
        }
    }
    pub fn toggle_view(&mut self) {
        self.set_view(match self.view {
            View::Repositories => View::Runs,
            View::Runs => View::Repositories,
        });
    }
    /// Cycle the active filter. On Repositories that's the status filter; on Runs it toggles the
    /// active-only view.
    pub fn cycle_filter(&mut self) {
        match self.view {
            View::Repositories => self.repo_filter = self.repo_filter.next(),
            View::Runs => self.runs_active_only = !self.runs_active_only,
        }
        self.clamp_selection();
        self.mark_dirty();
    }

    // --- overlays ---
    pub fn toggle_help(&mut self) {
        self.show_help = !self.show_help;
        self.mark_dirty();
    }
    pub fn toast_info(&mut self, text: impl Into<String>) {
        self.set_toast(text, ToastKind::Info);
    }
    pub fn toast_success(&mut self, text: impl Into<String>) {
        self.set_toast(text, ToastKind::Success);
    }
    pub fn toast_error(&mut self, text: impl Into<String>) {
        self.set_toast(text, ToastKind::Error);
    }
    fn set_toast(&mut self, text: impl Into<String>, kind: ToastKind) {
        self.toast = Some(Toast {
            text: text.into(),
            kind,
            shown_at: Instant::now(),
        });
        self.mark_dirty();
    }
    /// Expire the toast if its TTL has elapsed. Returns true if it changed (needs redraw).
    pub fn tick_toast(&mut self, now: Instant) -> bool {
        if let Some(t) = &self.toast {
            if now.duration_since(t.shown_at) >= TOAST_TTL {
                self.toast = None;
                self.mark_dirty();
                return true;
            }
        }
        false
    }

    /// Ask the operator to confirm the given action (guards approve/deny/cancel).
    pub fn ask_confirm(&mut self, prompt: impl Into<String>, action: PendingAction) {
        self.confirm = Some(Confirm {
            prompt: prompt.into(),
            action,
        });
        self.mark_dirty();
    }
    /// Take the pending action if confirmed (Enter/y); clears the prompt either way.
    pub fn resolve_confirm(&mut self, accepted: bool) -> Option<PendingAction> {
        self.mark_dirty();
        let confirm = self.confirm.take()?;
        accepted.then_some(confirm.action)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Claims;

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

    #[test]
    fn confirm_returns_action_only_when_accepted() {
        let mut a = app();
        a.ask_confirm("approve?", PendingAction::Approve(5));
        assert!(a.confirm.is_some());
        let action = a.resolve_confirm(false);
        assert!(action.is_none(), "declined confirm yields no action");
        assert!(a.confirm.is_none(), "prompt cleared on decline");

        a.ask_confirm("approve?", PendingAction::Approve(5));
        match a.resolve_confirm(true) {
            Some(PendingAction::Approve(5)) => {}
            other => panic!("expected Approve(5), got {other:?}"),
        }
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
