//! The `App` Model: the plain data (which view is active, the loaded rows, selection, the auth
//! status, redraw/loading flags) plus the constructor and the small set of read-only queries over it.
//! Kept free of I/O and rendering so the state is unit-testable; the event loop (`tui::update`) drives
//! transitions in `super::update`, and the renderer (`tui::ui`) only ever reads it.

use super::detail::DetailState;
use super::repo_settings::RepoSettingsState;
use super::types::{Confirm, RepoFilter, Toast, View};
use crate::api::{Me, RepositoryRow, TaskRow};
use crate::theme::{Theme, ThemeKind};

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

    /// The open Run Detail page, if any (view == Detail). `None` on the list views.
    pub detail: Option<DetailState>,

    /// The open Repo Settings page, if any (view == RepoSettings, story #500). `None` on the list
    /// views.
    pub repo_settings: Option<RepoSettingsState>,

    /// Whether crossterm mouse capture is active. Toggled with `m` so the operator can fall back to
    /// the terminal's native text selection (mouse capture steals it). The event loop enables/disables
    /// capture to match; state just tracks the intent + drives the status-bar indicator.
    pub mouse_enabled: bool,

    pub toast: Option<Toast>,
    pub confirm: Option<Confirm>,
    pub show_help: bool,

    /// The active color theme (cyclable at runtime with `t`).
    pub theme_kind: ThemeKind,
    /// A monotonically advancing frame counter, so the loading spinner animates across redraws.
    pub spinner_frame: usize,
    /// True while a background fetch is in flight (drives the status-bar spinner).
    pub loading: bool,

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
    pub(super) needs_view_refresh: bool,
    /// Bumped whenever state changes in a way that needs a redraw.
    pub(super) dirty: bool,
}

impl App {
    pub fn new(
        me: Me,
        api_host: String,
        token_expires_at: i64,
        refresh_token: Option<String>,
        theme_kind: ThemeKind,
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
            detail: None,
            repo_settings: None,
            mouse_enabled: true,
            toast: None,
            confirm: None,
            show_help: false,
            theme_kind,
            spinner_frame: 0,
            loading: false,
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
    /// `review:read` gates the detail view's review + transcript fetches.
    pub fn can_review_read(&self) -> bool {
        self.me.as_ref().is_some_and(|m| m.can("review:read"))
    }
    /// `repo:configure` gates the repo-settings page's SAVE action (story #500, ADR-0109) — the page
    /// itself still opens read-only without it, mirroring the detail page's `permission_denied` shape.
    pub fn can_configure_preset(&self) -> bool {
        self.me.as_ref().is_some_and(|m| m.can("repo:configure"))
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

    // --- theme + spinner ---
    /// The resolved palette for the active theme.
    pub fn theme(&self) -> Theme {
        Theme::from_kind(self.theme_kind)
    }
    /// Advance the spinner animation by one step (called on the UI tick).
    pub fn tick_spinner(&mut self) {
        if self.loading {
            self.spinner_frame = self.spinner_frame.wrapping_add(1);
            self.mark_dirty();
        }
    }
    /// Flag whether a fetch is in flight (drives the status-bar spinner).
    pub fn set_loading(&mut self, loading: bool) {
        if self.loading != loading {
            self.loading = loading;
            self.mark_dirty();
        }
    }
}
