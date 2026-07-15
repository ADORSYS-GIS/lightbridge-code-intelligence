//! The Update layer: `App`'s screen/selection/overlay **transitions** — turning an operator intent
//! (a decoded key, a mutation result, a timer tick) into a new [`App`] state. Split from
//! [`super::state`] (the plain Model data + simple queries) so the "what happens when…" logic reads
//! as its own surface. Lives in a second `impl App` block — legal in Rust as long as the type is
//! defined once in the crate — rather than duplicating/wrapping the struct.

use super::detail::DetailState;
use super::state::App;
use super::types::{Confirm, PendingAction, Toast, ToastKind, View};
use crate::api::{RepositoryRow, TaskRow};
use crate::theme::ButtonKind;
use std::time::{Duration, Instant};

/// How long a toast stays up.
const TOAST_TTL: Duration = Duration::from_secs(4);

impl App {
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
        if let Some(t) = &self.toast
            && now.duration_since(t.shown_at) >= TOAST_TTL
        {
            self.toast = None;
            self.mark_dirty();
            return true;
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

    /// Cycle to the next built-in theme (the `t` key).
    pub fn cycle_theme(&mut self) {
        self.theme_kind = self.theme_kind.next();
        self.toast_info(format!("theme: {}", self.theme_kind.name()));
        self.mark_dirty();
    }
}
