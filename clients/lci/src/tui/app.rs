//! The application state machine: which view is active, the loaded rows, selection, filters, the
//! toast/confirm/help overlays, and the auth status. Kept free of I/O and rendering so the state
//! transitions are unit-testable; the event loop (`tui::run`) drives it and the renderer (`tui::ui`)
//! reads it.

use crate::api::{Me, RepositoryRow, ReviewRow, TaskRow, TranscriptRow};
use crate::theme::{ButtonKind, Theme, ThemeKind};
use std::time::{Duration, Instant};

/// The operator views. `Detail` is a "page" opened from a selected Runs row (Enter / l / →).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Repositories,
    Runs,
    Detail,
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

/// A pending confirmation prompt (`a`/`d`/`c` ask before acting). Carries which of the two buttons
/// currently has focus so the renderer can highlight it and Enter picks the right choice.
#[derive(Debug, Clone)]
pub struct Confirm {
    /// The target described in a full sentence (e.g. "Approve vymalo/lci?").
    pub prompt: String,
    /// A one-line consequence note shown under the prompt (may be empty).
    pub detail: String,
    /// The verb + kind for the affirmative button (e.g. "Approve", Primary).
    pub verb: String,
    pub verb_kind: ButtonKind,
    pub action: PendingAction,
    /// Which button is focused. `true` = the affirmative button, `false` = Cancel.
    pub confirm_focused: bool,
}

impl Confirm {
    /// Move focus to the other button (Left/Right/Tab all toggle between exactly two).
    pub fn toggle_focus(&mut self) {
        self.confirm_focused = !self.confirm_focused;
    }
}

/// The action a confirmation will trigger once accepted.
#[derive(Debug, Clone)]
pub enum PendingAction {
    Approve(i64),
    Deny(i64),
    Cancel(uuid::Uuid),
}

/// The Run Detail "page": one task's metadata + review + a live-tailing transcript. Opened from a
/// Runs row and torn down on Esc/back, so the poll only runs while the page is visible.
///
/// Scroll semantics (the "live log tail"): the transcript renders newest-at-bottom. When new turns
/// arrive we **autoscroll** to the bottom — *unless* the operator has scrolled up, in which case we
/// hold their position and count the unseen turns (`new_since_scroll`) for a `▼ N new` indicator.
/// `G`/End jumps to the bottom and re-engages autoscroll.
pub struct DetailState {
    /// The task this page is about.
    pub task_id: uuid::Uuid,
    /// Full metadata (seeded from the Runs row, refreshed by the poll).
    pub task: TaskRow,
    /// The review, once fetched. `None` = not fetched yet or none recorded (see `review_loaded`).
    pub review: Option<ReviewRow>,
    /// True once the review fetch has resolved (so we can distinguish "loading" from "none recorded").
    pub review_loaded: bool,
    /// The transcript turns, ordered by `seq` (dedup on append).
    pub transcript: Vec<TranscriptRow>,
    /// True once the first transcript fetch resolved (distinguishes "loading" from genuinely empty).
    pub transcript_loaded: bool,
    /// Top line of the transcript viewport (line-granular scroll offset).
    pub scroll: u16,
    /// Total wrapped content-line count measured by the last render (the renderer knows the wrap
    /// width, so it writes this back through a `Cell` during the otherwise-immutable draw). Read by
    /// [`Self::sync_after_render`] to clamp + pin.
    pub content_lines: std::cell::Cell<u16>,
    /// Height of the transcript viewport's inner area from the last render (also renderer-written).
    pub viewport_lines: std::cell::Cell<u16>,
    /// When true, new turns keep the view pinned to the bottom. Disengaged when the user scrolls up,
    /// re-engaged by `G`/End (or scrolling back to the bottom).
    pub autoscroll: bool,
    /// Count of turns that arrived while the user was scrolled up (for the `▼ N new` badge).
    pub new_since_scroll: usize,
    /// True while the task is in a non-terminal status and we're polling (`● live`).
    pub live: bool,
    /// Set when the caller lacks `review:read`: we skip the fetch and show an inline notice instead.
    pub permission_denied: bool,
}

impl DetailState {
    /// Open a detail page for `task`. `can_read` gates the review/transcript fetch on `review:read`.
    pub fn new(task: TaskRow, can_read: bool) -> Self {
        let live = task.is_active();
        Self {
            task_id: task.id,
            live,
            task,
            review: None,
            review_loaded: false,
            transcript: Vec::new(),
            transcript_loaded: false,
            scroll: 0,
            content_lines: std::cell::Cell::new(0),
            viewport_lines: std::cell::Cell::new(0),
            autoscroll: true,
            new_since_scroll: 0,
            permission_denied: !can_read,
        }
    }

    /// Whether the detail poll should run: the task is still live and we're allowed to read it.
    pub fn should_poll(&self) -> bool {
        self.live && !self.permission_denied
    }

    /// The maximum valid scroll offset given the last-known content + viewport sizes.
    pub fn max_scroll(&self) -> u16 {
        self.content_lines
            .get()
            .saturating_sub(self.viewport_lines.get())
    }

    /// Called by the renderer (during the immutable `draw`) to record the geometry it measured for
    /// this frame. Stored in `Cell`s so the mutable [`Self::sync_after_render`] can apply autoscroll
    /// on the next loop turn without the renderer needing `&mut`.
    pub fn record_geometry(&self, content_lines: u16, viewport_lines: u16) {
        self.content_lines.set(content_lines);
        self.viewport_lines.set(viewport_lines);
    }

    /// After a render, reconcile the scroll offset with the freshly-measured geometry: pin to the
    /// bottom while autoscroll is engaged, otherwise just clamp so a shrunk transcript can't scroll
    /// past the end. Returns true if the offset moved (a redraw is warranted).
    pub fn sync_after_render(&mut self) -> bool {
        let target = if self.autoscroll {
            self.max_scroll()
        } else {
            self.scroll.min(self.max_scroll())
        };
        let moved = target != self.scroll;
        self.scroll = target;
        moved
    }

    /// Test/geometry helper: set the measured geometry and immediately reconcile the offset. Mirrors
    /// what `record_geometry` + `sync_after_render` do across a render/loop boundary.
    #[cfg(test)]
    pub fn set_geometry(&mut self, content_lines: u16, viewport_lines: u16) {
        self.record_geometry(content_lines, viewport_lines);
        self.sync_after_render();
    }

    /// Merge freshly-fetched turns, deduped by `seq`. Returns how many were genuinely new. When the
    /// user is scrolled up (autoscroll off), the new count feeds the `▼ N new` badge.
    pub fn merge_transcript(&mut self, incoming: Vec<TranscriptRow>) -> usize {
        let mut added = 0;
        for row in incoming {
            if !self.transcript.iter().any(|t| t.seq == row.seq) {
                self.transcript.push(row);
                added += 1;
            }
        }
        if added > 0 {
            // Keep the canonical newest-at-bottom order regardless of fetch ordering.
            self.transcript.sort_by_key(|t| t.seq);
            if !self.autoscroll {
                self.new_since_scroll += added;
            }
        }
        self.transcript_loaded = true;
        added
    }

    /// Reflect a refreshed task row (status may have advanced). Flips `live` off on a terminal status.
    pub fn set_task(&mut self, task: TaskRow) {
        self.live = task.is_active();
        self.task = task;
    }

    /// Scroll up by `n` lines — disengages autoscroll (the operator is inspecting history).
    pub fn scroll_up(&mut self, n: u16) {
        self.scroll = self.scroll.saturating_sub(n);
        self.autoscroll = false;
    }

    /// Scroll down by `n` lines. Re-engages autoscroll (and clears the new-badge) once the bottom is
    /// reached, so scrolling back down resumes the tail.
    pub fn scroll_down(&mut self, n: u16) {
        self.scroll = self.scroll.saturating_add(n).min(self.max_scroll());
        if self.scroll >= self.max_scroll() {
            self.autoscroll = true;
            self.new_since_scroll = 0;
        }
    }

    /// Jump to the top (disengages autoscroll).
    pub fn scroll_top(&mut self) {
        self.scroll = 0;
        self.autoscroll = false;
    }

    /// Jump to the bottom and re-engage autoscroll (`G`/End), clearing the new-turn badge.
    pub fn scroll_bottom(&mut self) {
        self.scroll = self.max_scroll();
        self.autoscroll = true;
        self.new_since_scroll = 0;
    }
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

    /// The open Run Detail page, if any (view == Detail). `None` on the list views.
    pub detail: Option<DetailState>,

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

    // --- run detail page ---
    /// Open the Run Detail page for the currently-selected task (Enter / l / →). No-op unless on the
    /// Runs view with a selected row. The fetch is gated on `review:read`; without it the page shows
    /// an inline notice instead of firing requests.
    pub fn open_detail(&mut self) {
        if self.view != View::Runs {
            return;
        }
        let Some(task) = self.selected_task().cloned() else {
            return;
        };
        self.detail = Some(DetailState::new(task, self.can_review_read()));
        self.view = View::Detail;
        self.mark_dirty();
    }

    /// Close the detail page and return to the Runs list (Esc / h / ←). Stops the poll (the poll gate
    /// checks `view == Detail`).
    pub fn close_detail(&mut self) {
        self.detail = None;
        self.view = View::Runs;
        self.mark_dirty();
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
            // The detail page has no list selection — j/k scroll the transcript instead.
            View::Detail => 0,
        }
    }
    fn current_selection(&self) -> usize {
        match self.view {
            View::Repositories => self.repo_selected,
            View::Runs => self.run_selected,
            View::Detail => 0,
        }
    }
    fn set_selection(&mut self, idx: usize) {
        match self.view {
            View::Repositories => self.repo_selected = idx,
            View::Runs => self.run_selected = idx,
            View::Detail => {}
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
        // Tab from the detail page returns to the Runs list first (then toggles as usual).
        if self.view == View::Detail {
            self.close_detail();
            return;
        }
        self.set_view(match self.view {
            View::Repositories => View::Runs,
            View::Runs | View::Detail => View::Repositories,
        });
    }
    /// Cycle the active filter. On Repositories that's the status filter; on Runs it toggles the
    /// active-only view. No-op on the detail page.
    pub fn cycle_filter(&mut self) {
        match self.view {
            View::Repositories => self.repo_filter = self.repo_filter.next(),
            View::Runs => self.runs_active_only = !self.runs_active_only,
            View::Detail => return,
        }
        self.clamp_selection();
        self.mark_dirty();
    }

    /// Toggle mouse capture (the `m` key). Returns the new state so the event loop can enable/disable
    /// crossterm capture to match. Surfaces a toast explaining the text-selection tradeoff.
    pub fn toggle_mouse(&mut self) -> bool {
        self.mouse_enabled = !self.mouse_enabled;
        if self.mouse_enabled {
            self.toast_info("mouse capture on — scroll works, native text-select off");
        } else {
            self.toast_info("mouse capture off — native text-select back on");
        }
        self.mark_dirty();
        self.mouse_enabled
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

    /// Ask the operator to confirm the given action (guards approve/deny/cancel). Focus defaults to
    /// the safe **Cancel** button, so a reflexive Enter never fires a destructive action.
    pub fn ask_confirm(
        &mut self,
        prompt: impl Into<String>,
        detail: impl Into<String>,
        verb: impl Into<String>,
        verb_kind: ButtonKind,
        action: PendingAction,
    ) {
        self.confirm = Some(Confirm {
            prompt: prompt.into(),
            detail: detail.into(),
            verb: verb.into(),
            verb_kind,
            action,
            confirm_focused: false,
        });
        self.mark_dirty();
    }

    /// Move focus between the confirm dialog's two buttons (Left/Right/Tab).
    pub fn confirm_toggle_focus(&mut self) {
        if let Some(c) = &mut self.confirm {
            c.toggle_focus();
            self.mark_dirty();
        }
    }

    /// Resolve the confirm dialog by pressing the focused button (Enter). Returns the action only if
    /// the affirmative button had focus; clears the prompt either way.
    pub fn resolve_confirm_focused(&mut self) -> Option<PendingAction> {
        self.mark_dirty();
        let confirm = self.confirm.take()?;
        confirm.confirm_focused.then_some(confirm.action)
    }

    /// Resolve the confirm dialog explicitly (Esc = decline, `y` = accept regardless of focus).
    pub fn resolve_confirm(&mut self, accepted: bool) -> Option<PendingAction> {
        self.mark_dirty();
        let confirm = self.confirm.take()?;
        accepted.then_some(confirm.action)
    }

    // --- theme + spinner ---
    /// The resolved palette for the active theme.
    pub fn theme(&self) -> Theme {
        Theme::from_kind(self.theme_kind)
    }
    /// Cycle to the next built-in theme (the `t` key).
    pub fn cycle_theme(&mut self) {
        self.theme_kind = self.theme_kind.next();
        self.toast_info(format!("theme: {}", self.theme_kind.name()));
        self.mark_dirty();
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
            ThemeKind::Midnight,
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
        assert_eq!(a.theme_kind, ThemeKind::Midnight);
        a.cycle_theme();
        assert_eq!(a.theme_kind, ThemeKind::Terminal);
        a.cycle_theme();
        a.cycle_theme();
        assert_eq!(a.theme_kind, ThemeKind::Midnight, "wraps back");
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
        }
    }

    fn turn(seq: i32) -> TranscriptRow {
        TranscriptRow {
            seq,
            role: "assistant".into(),
            content: Some(format!("turn {seq}")),
            tool_calls: None,
            tool_name: None,
            prompt_tokens: Some(10),
            completion_tokens: Some(2),
            model: None,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
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
        assert!(d.live, "a running task tails live");
        assert!(d.should_poll() || d.permission_denied);
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
        assert!(!d.should_poll(), "denied read must not poll");
    }

    #[test]
    fn detail_autoscroll_pins_to_bottom_as_turns_arrive() {
        let mut d = DetailState::new(task_row("running"), true);
        d.merge_transcript(vec![turn(0), turn(1), turn(2)]);
        // 10 content lines in a 4-line viewport → max scroll 6; autoscroll pins there.
        d.set_geometry(10, 4);
        assert!(d.autoscroll);
        assert_eq!(d.scroll, 6, "pinned to the bottom");

        // A new turn extends the content; autoscroll keeps us pinned.
        let added = d.merge_transcript(vec![turn(3)]);
        assert_eq!(added, 1);
        d.set_geometry(13, 4);
        assert_eq!(d.scroll, 9, "still pinned after growth");
        assert_eq!(d.new_since_scroll, 0, "no badge while pinned");
    }

    #[test]
    fn detail_hold_position_and_count_new_when_scrolled_up() {
        let mut d = DetailState::new(task_row("running"), true);
        d.merge_transcript((0..5).map(turn).collect());
        d.set_geometry(20, 5); // max scroll 15, pinned at 15
        assert_eq!(d.scroll, 15);

        // User scrolls up → autoscroll disengages, position held.
        d.scroll_up(6);
        assert!(!d.autoscroll);
        assert_eq!(d.scroll, 9);

        // New turns arrive: position must NOT jump; the badge counts them.
        d.merge_transcript(vec![turn(5), turn(6)]);
        d.set_geometry(28, 5);
        assert_eq!(d.scroll, 9, "held while scrolled up");
        assert_eq!(d.new_since_scroll, 2, "▼ 2 new");

        // G/End re-engages autoscroll, clears the badge, jumps to bottom.
        d.scroll_bottom();
        assert!(d.autoscroll);
        assert_eq!(d.new_since_scroll, 0);
        assert_eq!(d.scroll, d.max_scroll());
    }

    #[test]
    fn detail_merge_dedupes_by_seq_and_orders() {
        let mut d = DetailState::new(task_row("running"), true);
        d.merge_transcript(vec![turn(2), turn(0)]);
        // Re-fetch overlaps seq 0 and 2, adds 1 and 3 (out of order).
        let added = d.merge_transcript(vec![turn(0), turn(3), turn(1), turn(2)]);
        assert_eq!(added, 2, "only the genuinely-new seqs counted");
        let seqs: Vec<i32> = d.transcript.iter().map(|t| t.seq).collect();
        assert_eq!(seqs, vec![0, 1, 2, 3], "sorted, no dupes");
    }

    #[test]
    fn detail_set_task_flips_live_off_on_terminal_status() {
        let mut d = DetailState::new(task_row("running"), true);
        assert!(d.live);
        d.set_task(task_row("succeeded"));
        assert!(!d.live, "terminal status stops the tail");
        assert!(!d.should_poll());
    }

    #[test]
    fn scroll_down_to_bottom_reengages_autoscroll() {
        let mut d = DetailState::new(task_row("running"), true);
        d.merge_transcript((0..5).map(turn).collect());
        d.set_geometry(20, 5);
        d.scroll_top();
        assert!(!d.autoscroll);
        assert_eq!(d.scroll, 0);
        // Scroll all the way back down re-engages the tail.
        d.scroll_down(100);
        assert!(d.autoscroll);
        assert_eq!(d.scroll, 15);
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
