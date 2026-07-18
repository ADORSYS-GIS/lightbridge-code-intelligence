//! The Update layer's terminal-facing half: translate a raw crossterm key/mouse event into a state
//! transition on [`App`] (and/or a spawned network request via [`super::message`]). Kept separate from
//! [`super::message`] (the async `Msg`/`Cmd` plumbing) and from `App` itself (the Model, in
//! `tui::app`) so "what does this keypress do" reads as its own surface — the TUI-idiomatic
//! Model/Update/View split, with `match` doing the "what key was pressed" / "what screen am I on"
//! dispatch throughout rather than `if`/`else if` chains.

use super::app::{App, PendingAction, View};
use super::message::{self, Msg};
use super::terminal::TerminalGuard;
use crate::api::{ApiClient, TaskRow};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use crossterm::execute;
use tokio::sync::mpsc;

/// Translate a keypress into a state change and/or a spawned request. `guard` is threaded so the `m`
/// mouse-toggle can enable/disable crossterm capture on the live terminal.
pub(super) fn handle_key(
    app: &mut App,
    key: KeyEvent,
    api: &ApiClient,
    tx: &mpsc::UnboundedSender<Msg>,
    guard: &mut TerminalGuard,
) {
    // A pending confirmation captures navigation + Enter/y/Esc/n first. Left/Right/Tab move focus
    // between the two buttons; Enter fires whichever is focused; `y` is a power-user accept regardless
    // of focus; Esc/`n` decline.
    if app.confirm.is_some() {
        match key.code {
            KeyCode::Left
            | KeyCode::Right
            | KeyCode::Tab
            | KeyCode::Char('h')
            | KeyCode::Char('l') => {
                app.confirm_toggle_focus();
            }
            KeyCode::Enter => {
                if let Some(action) = app.resolve_confirm_focused() {
                    message::dispatch_action(app, action, api, tx);
                }
            }
            KeyCode::Char('y') => {
                if let Some(action) = app.resolve_confirm(true) {
                    message::dispatch_action(app, action, api, tx);
                }
            }
            KeyCode::Esc | KeyCode::Char('n') => {
                app.resolve_confirm(false);
            }
            _ => {}
        }
        return;
    }

    // Ctrl-C always quits.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        app.should_quit = true;
        return;
    }

    // The help overlay captures any key to dismiss.
    if app.show_help {
        app.toggle_help();
        return;
    }

    // `m` toggles mouse capture from any view (so the operator can grab native text selection).
    if key.code == KeyCode::Char('m') {
        let enabled = app.toggle_mouse();
        set_mouse_capture(guard, enabled);
        return;
    }

    // The detail page has its own key map (scroll + back); handle it before the list keys.
    if app.view == View::Detail {
        handle_detail_key(app, key, api, tx);
        return;
    }

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
        KeyCode::Char('?') => app.toggle_help(),
        KeyCode::Tab => {
            app.toggle_view();
            message::spawn_refresh_current_view(app, api, tx);
        }
        KeyCode::Char('1') => {
            app.set_view(View::Repositories);
            message::spawn_refresh_current_view(app, api, tx);
        }
        KeyCode::Char('2') => {
            app.set_view(View::Runs);
            message::spawn_refresh_current_view(app, api, tx);
        }
        KeyCode::Down | KeyCode::Char('j') => app.select_next(),
        KeyCode::Up | KeyCode::Char('k') => app.select_prev(),
        // Enter / l / → opens the selected run's detail page (Runs view only).
        KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right if app.view == View::Runs => {
            app.open_detail();
            // Read the target out of the immutable borrow, then fetch (spawn needs `&mut app`).
            let target = app
                .detail
                .as_ref()
                .filter(|d| !d.permission_denied)
                .map(|d| d.task_id);
            if let Some(id) = target {
                message::spawn_detail_fetch(id, app, api, tx);
            }
        }
        KeyCode::Char('r') => {
            app.toast_info("refreshing…");
            message::spawn_refresh_current_view(app, api, tx);
        }
        KeyCode::Char('f') => {
            app.cycle_filter();
            message::spawn_refresh_current_view(app, api, tx);
        }
        KeyCode::Char('t') => app.cycle_theme(),
        KeyCode::Char('a') => request_approve(app),
        KeyCode::Char('d') => request_deny(app),
        KeyCode::Char('c') => request_cancel(app),
        _ => {}
    }
}

/// The detail page's key map: refresh the task + review, and back out to the Runs list. Run
/// observability moved to Loki (epic #459), so the page is a static status + review summary — there is
/// no transcript to scroll.
fn handle_detail_key(
    app: &mut App,
    key: KeyEvent,
    api: &ApiClient,
    tx: &mpsc::UnboundedSender<Msg>,
) {
    match key.code {
        KeyCode::Esc | KeyCode::Char('h') | KeyCode::Left | KeyCode::Char('q') => {
            app.close_detail();
        }
        KeyCode::Char('?') => app.toggle_help(),
        KeyCode::Char('t') => app.cycle_theme(),
        KeyCode::Char('r') => {
            let target = app
                .detail
                .as_ref()
                .filter(|d| !d.permission_denied)
                .map(|d| d.task_id);
            app.toast_info("refreshing…");
            if let Some(id) = target {
                message::spawn_detail_fetch(id, app, api, tx);
            }
        }
        _ => {}
    }
}

/// Handle a mouse event: wheel scroll drives the list selection (the detail page has nothing to
/// scroll). A left click on a Runs row selects it.
pub(super) fn handle_mouse(app: &mut App, m: MouseEvent) {
    match m.kind {
        MouseEventKind::ScrollDown => app.select_next(),
        MouseEventKind::ScrollUp => app.select_prev(),
        MouseEventKind::Down(MouseButton::Left) => {
            // Nice-to-have: a left click on the Runs list opens the row under the cursor's detail.
            // We keep it minimal — a click just selects the nearest row by not changing selection
            // (row-precise hit-testing needs the table's layout rects, which we don't thread here).
            // Left intentionally as a no-op to avoid a janky half-feature.
        }
        _ => {}
    }
}

/// Enable or disable crossterm mouse capture on the live terminal to match the app's toggle. Failure
/// is non-fatal (best-effort, like restore) — the toast already told the operator the intended state.
fn set_mouse_capture(guard: &mut TerminalGuard, enabled: bool) {
    use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
    let backend = guard.terminal.backend_mut();
    let _ = if enabled {
        execute!(backend, EnableMouseCapture)
    } else {
        execute!(backend, DisableMouseCapture)
    };
}

/// `a` on Repositories → confirm approve (gated by `repo:approve`).
fn request_approve(app: &mut App) {
    if app.view != View::Repositories {
        return;
    }
    if !app.can_approve() {
        app.toast_error("you lack repo:approve");
        return;
    }
    if let Some(repo) = app.selected_repo() {
        let (id, label) = (repo.id, format!("{}/{}", repo.owner, repo.name));
        app.ask_confirm(
            format!("Approve {label}?"),
            "Opens the gate and triggers indexing.",
            "Approve",
            crate::theme::ButtonKind::Primary,
            PendingAction::Approve(id),
        );
    }
}

/// `d` on Repositories → confirm deny (gated by `repo:deny`).
fn request_deny(app: &mut App) {
    if app.view != View::Repositories {
        return;
    }
    if !app.can_deny() {
        app.toast_error("you lack repo:deny");
        return;
    }
    if let Some(repo) = app.selected_repo() {
        let (id, label) = (repo.id, format!("{}/{}", repo.owner, repo.name));
        app.ask_confirm(
            format!("Deny {label}?"),
            "Disables it and PURGES its index data.",
            "Deny",
            crate::theme::ButtonKind::Danger,
            PendingAction::Deny(id),
        );
    }
}

/// `c` on Runs → confirm cancel (gated by `task:cancel`).
fn request_cancel(app: &mut App) {
    if app.view != View::Runs {
        return;
    }
    if !app.can_cancel() {
        app.toast_error("you lack task:cancel");
        return;
    }
    if let Some(task) = app.selected_task() {
        if !task.is_active() {
            app.toast_error("that run is already finished");
            return;
        }
        let (id, label) = (task.id, target_label_for(task));
        app.ask_confirm(
            format!("Cancel the {label} run?"),
            "Signals the running task to stop.",
            "Cancel run",
            crate::theme::ButtonKind::Danger,
            PendingAction::Cancel(id),
        );
    }
}

/// A compact human label for a task target, reused in the cancel confirm sentence.
fn target_label_for(t: &TaskRow) -> String {
    match (&t.repo_owner, &t.repo_name) {
        (Some(o), Some(n)) => format!("{o}/{n}"),
        _ => format!("repo#{}", t.repository_id),
    }
}
