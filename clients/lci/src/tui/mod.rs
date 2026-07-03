//! The async TUI event loop. Wires crossterm input, a periodic refresh timer, and an mpsc channel of
//! async API results into the [`App`] state machine, and renders each frame via [`ui::draw`].
//!
//! Networking never runs on the render path: key actions spawn tasks that post their result back
//! over the channel, so the UI stays responsive.

pub(crate) mod app;
mod terminal;
pub(crate) mod ui;

pub use app::App;

use crate::api::{ApiClient, Me, RepositoryRow, ReviewRow, TaskRow, TranscriptRow};
use crate::auth::{self, EXPIRY_SKEW_SECS};
use crate::config::Config;
use anyhow::Result;
use app::{PendingAction, View};
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::execute;
use futures::StreamExt;
use std::time::{Duration, Instant};
use terminal::TerminalGuard;
use tokio::sync::mpsc;
use uuid::Uuid;

/// Auto-refresh cadence.
const REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// Live-tail poll cadence for the open detail page (2–3s per the ticket). Faster than the list
/// refresh so a running agent's new turns show promptly.
const DETAIL_POLL_INTERVAL: Duration = Duration::from_millis(2500);

/// A result posted back from a spawned async request task.
enum Msg {
    Repos(Result<Vec<RepositoryRow>>),
    Tasks(Result<Vec<TaskRow>>),
    /// A repo mutation (approve/deny) completed; carries the friendly verb for the toast.
    RepoAction {
        verb: &'static str,
        result: Result<RepositoryRow>,
    },
    /// A cancel completed.
    Cancelled(Result<()>),
    /// Background refresh produced a new token (or failed → set the re-auth flag).
    TokenRefreshed(Result<auth::StoredToken>),
    /// A detail-page fetch resolved (task metadata + review + transcript), carrying the task id it
    /// was fetched for so a stale result for a closed/other page is ignored.
    Detail {
        task_id: Uuid,
        task: Result<TaskRow>,
        review: Result<Option<ReviewRow>>,
        transcript: Result<Vec<TranscriptRow>>,
    },
    /// A lighter live-tail poll: just the task status + transcript (no review re-fetch).
    DetailTail {
        task_id: Uuid,
        task: Result<TaskRow>,
        transcript: Result<Vec<TranscriptRow>>,
    },
}

/// Run the TUI until the operator quits. `api` is already authenticated; `me` and `token_expires_at`
/// seed the status bar. `cfg` + `http` power background refresh.
pub async fn run(
    api: ApiClient,
    me: Me,
    token_expires_at: i64,
    cfg: Config,
    http: reqwest::Client,
    refresh_token: Option<String>,
    theme_kind: crate::theme::ThemeKind,
) -> Result<()> {
    let mut guard = TerminalGuard::enter()?;
    let mut app = App::new(me, api.host(), token_expires_at, refresh_token, theme_kind);

    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
    let mut input = EventStream::new();
    let mut refresh_timer = tokio::time::interval(REFRESH_INTERVAL);
    // The first tick fires immediately; we also kick an initial load below.
    let mut ui_tick = tokio::time::interval(Duration::from_millis(250));
    // The live-tail poll for an open detail page (fires only while one is open + still live).
    let mut detail_poll = tokio::time::interval(DETAIL_POLL_INTERVAL);

    // Kick the initial data + a token-refresh watchdog state.
    app.set_loading(true);
    spawn_refresh_current_view(&app, &api, &tx);
    let mut last_refresh_attempt = Instant::now();

    // Initial draw.
    guard.terminal.draw(|f| ui::draw(f, &app))?;

    loop {
        tokio::select! {
            // --- keyboard / terminal input ---
            maybe_event = input.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        handle_key(&mut app, key, &api, &tx, &mut guard);
                    }
                    Some(Ok(Event::Mouse(m))) => handle_mouse(&mut app, m),
                    Some(Ok(Event::Resize(_, _))) => app.mark_dirty(),
                    Some(Err(_)) | None => break, // input stream closed
                    _ => {}
                }
            }

            // --- periodic auto-refresh of the active view ---
            _ = refresh_timer.tick() => {
                app.set_loading(true);
                spawn_refresh_current_view(&app, &api, &tx);
                maybe_spawn_token_refresh(&mut app, &cfg, &http, &tx, &mut last_refresh_attempt);
            }

            // --- live-tail poll for an open detail page (status + transcript) ---
            _ = detail_poll.tick() => {
                // `should_poll` includes the in-flight guard, so a slow backend can't stack polls.
                if let Some(d) = app.detail.as_mut() {
                    if d.should_poll() {
                        let id = d.task_id;
                        d.tail_in_flight = true;
                        spawn_detail_tail(id, &api, &tx);
                    }
                }
            }

            // --- lightweight UI tick (toast expiry + spinner + token countdown redraw) ---
            _ = ui_tick.tick() => {
                app.tick_toast(Instant::now());
                app.tick_spinner();
                // The token countdown changes every second; keep it live.
                app.mark_dirty();
            }

            // --- async API results ---
            Some(msg) = rx.recv() => {
                match apply_msg(&mut app, msg) {
                    FollowUp::None => {}
                    // Swap the live bearer so subsequent requests use the refreshed token.
                    FollowUp::SwapBearer(access) => api.set_bearer(access).await,
                    // Re-fetch the current view now (after a successful mutation).
                    FollowUp::RefreshView => app.request_view_refresh(),
                }
            }
        }

        // A successful mutation (or a swapped bearer) asked for an immediate re-fetch.
        if app.take_view_refresh() {
            spawn_refresh_current_view(&app, &api, &tx);
        }

        if app.should_quit {
            break;
        }
        if app.take_dirty() {
            guard.terminal.draw(|f| ui::draw(f, &app))?;
            // The detail renderer measured the transcript geometry into `DetailState`'s cells during
            // the draw; reconcile the scroll offset (autoscroll pin / clamp) now. If it moved, a
            // follow-up redraw shows the corrected position without waiting for the next event.
            if let Some(d) = app.detail.as_mut() {
                if d.sync_after_render() {
                    guard.terminal.draw(|f| ui::draw(f, &app))?;
                }
            }
        }
    }

    // `guard` drops here → terminal restored (mouse capture disabled included).
    Ok(())
}

/// Translate a keypress into a state change and/or a spawned request. `guard` is threaded so the `m`
/// mouse-toggle can enable/disable crossterm capture on the live terminal.
fn handle_key(
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
                    dispatch_action(app, action, api, tx);
                }
            }
            KeyCode::Char('y') => {
                if let Some(action) = app.resolve_confirm(true) {
                    dispatch_action(app, action, api, tx);
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
            spawn_refresh_current_view(app, api, tx);
        }
        KeyCode::Char('1') => {
            app.set_view(View::Repositories);
            spawn_refresh_current_view(app, api, tx);
        }
        KeyCode::Char('2') => {
            app.set_view(View::Runs);
            spawn_refresh_current_view(app, api, tx);
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
                spawn_detail_fetch(id, app, api, tx);
            }
        }
        KeyCode::Char('r') => {
            app.toast_info("refreshing…");
            spawn_refresh_current_view(app, api, tx);
        }
        KeyCode::Char('f') => {
            app.cycle_filter();
            spawn_refresh_current_view(app, api, tx);
        }
        KeyCode::Char('t') => app.cycle_theme(),
        KeyCode::Char('a') => request_approve(app),
        KeyCode::Char('d') => request_deny(app),
        KeyCode::Char('c') => request_cancel(app),
        _ => {}
    }
}

/// The detail page's key map: scroll the transcript, jump top/bottom (re-engaging the live tail),
/// refresh, and back out to the Runs list.
fn handle_detail_key(
    app: &mut App,
    key: KeyEvent,
    api: &ApiClient,
    tx: &mpsc::UnboundedSender<Msg>,
) {
    // A page = the transcript viewport height; fall back to a sensible default if not yet measured.
    let page = app
        .detail
        .as_ref()
        .map(|d| d.viewport_lines.get().max(1))
        .unwrap_or(10);

    // Scroll actions mutate only `app.detail`; do them in a scoped borrow so the `app.*` calls
    // (mark_dirty / close_detail / toasts) below don't collide with a held `&mut d`.
    match key.code {
        KeyCode::Esc | KeyCode::Char('h') | KeyCode::Left | KeyCode::Char('q') => {
            app.close_detail();
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if let Some(d) = app.detail.as_mut() {
                d.scroll_down(1);
            }
            app.mark_dirty();
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if let Some(d) = app.detail.as_mut() {
                d.scroll_up(1);
            }
            app.mark_dirty();
        }
        KeyCode::PageDown => {
            if let Some(d) = app.detail.as_mut() {
                d.scroll_down(page);
            }
            app.mark_dirty();
        }
        KeyCode::PageUp => {
            if let Some(d) = app.detail.as_mut() {
                d.scroll_up(page);
            }
            app.mark_dirty();
        }
        KeyCode::Char('g') | KeyCode::Home => {
            if let Some(d) = app.detail.as_mut() {
                d.scroll_top();
            }
            app.mark_dirty();
        }
        KeyCode::Char('G') | KeyCode::End => {
            if let Some(d) = app.detail.as_mut() {
                d.scroll_bottom();
            }
            app.mark_dirty();
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
                spawn_detail_fetch(id, app, api, tx);
            }
        }
        _ => {}
    }
}

/// Handle a mouse event: wheel scroll drives the focused scrollable pane (the transcript in detail
/// view; otherwise the list selection). A left click on a Runs row selects it.
fn handle_mouse(app: &mut App, m: crossterm::event::MouseEvent) {
    match m.kind {
        MouseEventKind::ScrollDown => match app.view {
            View::Detail => {
                if let Some(d) = app.detail.as_mut() {
                    d.scroll_down(3);
                    app.mark_dirty();
                }
            }
            _ => app.select_next(),
        },
        MouseEventKind::ScrollUp => match app.view {
            View::Detail => {
                if let Some(d) = app.detail.as_mut() {
                    d.scroll_up(3);
                    app.mark_dirty();
                }
            }
            _ => app.select_prev(),
        },
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

/// Spawn the network call for a confirmed action.
fn dispatch_action(
    app: &mut App,
    action: PendingAction,
    api: &ApiClient,
    tx: &mpsc::UnboundedSender<Msg>,
) {
    let api = api.clone();
    let tx = tx.clone();
    match action {
        PendingAction::Approve(id) => {
            app.toast_info("approving…");
            tokio::spawn(async move {
                let result = api.approve(id).await;
                let _ = tx.send(Msg::RepoAction {
                    verb: "approved",
                    result,
                });
            });
        }
        PendingAction::Deny(id) => {
            app.toast_info("denying…");
            tokio::spawn(async move {
                let result = api.deny(id).await;
                let _ = tx.send(Msg::RepoAction {
                    verb: "denied",
                    result,
                });
            });
        }
        PendingAction::Cancel(id) => {
            app.toast_info("cancelling…");
            tokio::spawn(async move {
                let result = api.cancel_task(id).await;
                let _ = tx.send(Msg::Cancelled(result));
            });
        }
    }
}

/// Spawn a refresh of whichever view is active, using its current filter.
fn spawn_refresh_current_view(app: &App, api: &ApiClient, tx: &mpsc::UnboundedSender<Msg>) {
    let api = api.clone();
    let tx = tx.clone();
    match app.view {
        View::Repositories => {
            let status = app.repo_filter.as_query().map(|s| s.to_string());
            tokio::spawn(async move {
                let result = api.list_repositories(status.as_deref()).await;
                let _ = tx.send(Msg::Repos(result));
            });
        }
        View::Runs => {
            tokio::spawn(async move {
                let result = api.list_tasks().await;
                let _ = tx.send(Msg::Tasks(result));
            });
        }
        // The detail page refreshes primarily via its own tail poll; the periodic list refresh also
        // re-fetches all three (task + review + transcript) so a freshly-posted review shows up. This
        // path is `&App`, so it spawns the fetch directly (no loading-flag flip — the tail already
        // keeps the page live); the interactive open/`r` paths go through `spawn_detail_fetch`.
        View::Detail => {
            if let Some(id) = app
                .detail
                .as_ref()
                .filter(|d| !d.permission_denied)
                .map(|d| d.task_id)
            {
                tokio::spawn(async move {
                    let (task, review, transcript) =
                        tokio::join!(api.get_task(id), api.get_review(id), api.get_transcript(id));
                    let _ = tx.send(Msg::Detail {
                        task_id: id,
                        task,
                        review,
                        transcript,
                    });
                });
            }
        }
    }
}

/// Spawn the full detail fetch: task metadata + review (404→None) + transcript, all for `id`. Flips
/// the loading flag on (the status-bar spinner) — cleared when the `Msg::Detail` result is folded in.
fn spawn_detail_fetch(id: Uuid, app: &mut App, api: &ApiClient, tx: &mpsc::UnboundedSender<Msg>) {
    app.set_loading(true);
    let (api, tx) = (api.clone(), tx.clone());
    tokio::spawn(async move {
        // Run the three fetches concurrently — they're independent GETs.
        let (task, review, transcript) =
            tokio::join!(api.get_task(id), api.get_review(id), api.get_transcript(id),);
        let _ = tx.send(Msg::Detail {
            task_id: id,
            task,
            review,
            transcript,
        });
    });
}

/// Spawn the lighter live-tail poll: task status + transcript only (no review re-fetch).
fn spawn_detail_tail(id: Uuid, api: &ApiClient, tx: &mpsc::UnboundedSender<Msg>) {
    let (api, tx) = (api.clone(), tx.clone());
    tokio::spawn(async move {
        let (task, transcript) = tokio::join!(api.get_task(id), api.get_transcript(id));
        let _ = tx.send(Msg::DetailTail {
            task_id: id,
            task,
            transcript,
        });
    });
}

/// If the token is within the skew window and we have a usable refresh token, spawn a background
/// refresh. Rate-limited so a burst of timer ticks doesn't stampede the IdP, and short-circuited once
/// `refresh_disabled` is latched (a prior fatal `invalid_grant`) so a dead token can't hot-loop.
fn maybe_spawn_token_refresh(
    app: &mut App,
    cfg: &Config,
    http: &reqwest::Client,
    tx: &mpsc::UnboundedSender<Msg>,
    last_attempt: &mut Instant,
) {
    // A prior fatal refresh already flipped us to re-auth; don't keep hammering the IdP.
    if app.refresh_disabled {
        return;
    }
    let Some(exp) = app.token_expires_at else {
        return;
    };
    let now = auth::now_unix();
    if exp - now > EXPIRY_SKEW_SECS {
        return; // still fresh
    }
    let Some(refresh) = app.refresh_token.clone() else {
        // No refresh token to use → re-auth, and latch so we stop re-checking every tick.
        app.reauth_needed = true;
        app.refresh_disabled = true;
        app.mark_dirty();
        return;
    };
    // At most one attempt per refresh interval.
    if last_attempt.elapsed() < REFRESH_INTERVAL {
        return;
    }
    *last_attempt = Instant::now();

    let (cfg, http, tx) = (cfg.clone(), http.clone(), tx.clone());
    tokio::spawn(async move {
        let result = auth::try_refresh(&http, &cfg, &refresh).await;
        let _ = tx.send(Msg::TokenRefreshed(result));
    });
}

/// A follow-up the event loop performs after folding a message into state — things that need `&api`
/// or must be `async` (swapping the live bearer, re-fetching a view). Keeps [`apply_msg`] sync.
enum FollowUp {
    None,
    /// A refresh produced a new access token → swap the client's live bearer to it.
    SwapBearer(String),
    /// A mutation succeeded → re-fetch the current view now (don't wait for the periodic refresh).
    RefreshView,
}

/// Fold an async result into the state, returning any follow-up the loop must run.
fn apply_msg(app: &mut App, msg: Msg) -> FollowUp {
    match msg {
        Msg::Repos(Ok(repos)) => {
            app.set_loading(false);
            app.set_repos(repos);
            FollowUp::None
        }
        Msg::Repos(Err(e)) => {
            app.set_loading(false);
            app.toast_error(format!("repos: {e}"));
            FollowUp::None
        }
        Msg::Tasks(Ok(tasks)) => {
            app.set_loading(false);
            app.set_tasks(tasks);
            FollowUp::None
        }
        Msg::Tasks(Err(e)) => {
            app.set_loading(false);
            app.toast_error(format!("runs: {e}"));
            FollowUp::None
        }
        Msg::RepoAction { verb, result } => match result {
            Ok(repo) => {
                app.toast_success(format!("{verb} {}/{}", repo.owner, repo.name));
                // Reflect the change immediately by re-fetching the list.
                FollowUp::RefreshView
            }
            Err(e) => {
                app.toast_error(format!("{verb} failed: {e}"));
                FollowUp::None
            }
        },
        Msg::Cancelled(Ok(())) => {
            app.toast_success("cancel requested");
            FollowUp::RefreshView
        }
        Msg::Cancelled(Err(e)) => {
            app.toast_error(format!("cancel failed: {e}"));
            FollowUp::None
        }
        Msg::Detail {
            task_id,
            task,
            review,
            transcript,
        } => {
            app.set_loading(false);
            // Ignore a result for a page the operator has since closed or replaced.
            if app.detail.as_ref().map(|d| d.task_id) != Some(task_id) {
                return FollowUp::None;
            }
            // Fold into the detail state in a scoped borrow, collecting any error text to toast after.
            let mut errors: Vec<String> = Vec::new();
            if let Some(d) = app.detail.as_mut() {
                if let Ok(t) = task {
                    d.set_task(t);
                }
                match review {
                    Ok(r) => {
                        d.review = r;
                        d.review_loaded = true;
                    }
                    Err(e) => {
                        d.review_loaded = true;
                        errors.push(format!("review: {e}"));
                    }
                }
                match transcript {
                    Ok(rows) => {
                        d.merge_transcript(rows);
                    }
                    Err(e) => {
                        d.transcript_loaded = true;
                        errors.push(format!("transcript: {e}"));
                    }
                }
            }
            if let Some(e) = errors.into_iter().next() {
                app.toast_error(e);
            }
            app.mark_dirty();
            FollowUp::None
        }
        Msg::DetailTail {
            task_id,
            task,
            transcript,
        } => {
            let Some(d) = app.detail.as_mut().filter(|d| d.task_id == task_id) else {
                // Page closed/replaced while the tail was in flight — nothing to clear (a fresh page
                // has its own `tail_in_flight = false`).
                return FollowUp::None;
            };
            // Clear the in-flight guard so the next tick may poll again.
            d.tail_in_flight = false;
            if let Ok(t) = task {
                d.set_task(t);
            }
            if let Ok(rows) = transcript {
                d.merge_transcript(rows);
            }
            app.mark_dirty();
            FollowUp::None
        }
        Msg::TokenRefreshed(Ok(token)) => {
            // Rotate ALL of: the live bearer (via the follow-up), the expiry, and the session refresh
            // token (Keycloak issues a new one and revokes the old — the next refresh must use it).
            app.token_expires_at = Some(token.expires_at);
            app.refresh_token = token.refresh_token.clone();
            app.reauth_needed = false;
            app.refresh_disabled = false;
            app.toast_info("token refreshed");
            FollowUp::SwapBearer(token.access_token)
        }
        Msg::TokenRefreshed(Err(_)) => {
            // Fatal: the refresh token is dead (rotated/expired/revoked). Surface re-auth and latch so
            // we don't re-fire it against the IdP every interval.
            app.reauth_needed = true;
            app.refresh_disabled = true;
            app.mark_dirty();
            FollowUp::None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Claims, Me};
    use anyhow::anyhow;

    fn test_app() -> App {
        let me = Me {
            claims: Claims {
                sub: "s".into(),
                email: None,
                preferred_username: Some("op".into()),
                name: None,
                exp: Some(0),
            },
            permissions: vec!["repo:approve".into(), "task:cancel".into()],
        };
        // Seeded with the ORIGINAL session refresh token.
        App::new(
            me,
            "api.test".into(),
            1_000,
            Some("rt-original".into()),
            crate::theme::ThemeKind::Midnight,
        )
    }

    fn stored(access: &str, refresh: Option<&str>, expires_at: i64) -> auth::StoredToken {
        auth::StoredToken {
            access_token: access.into(),
            refresh_token: refresh.map(String::from),
            token_type: "Bearer".into(),
            scope: None,
            expires_at,
            obtained_at: 0,
            id_token: None,
        }
    }

    #[test]
    fn successful_refresh_swaps_bearer_and_rotates_refresh_token() {
        let mut app = test_app();
        let token = stored("access-NEW", Some("rt-ROTATED"), 9_999);

        let follow = apply_msg(&mut app, Msg::TokenRefreshed(Ok(token)));

        // The loop is told to swap the live bearer to the new access token.
        match follow {
            FollowUp::SwapBearer(access) => assert_eq!(access, "access-NEW"),
            _ => panic!("expected SwapBearer with the new access token"),
        }
        // Expiry advanced, session refresh token ROTATED to the new one, re-auth cleared.
        assert_eq!(app.token_expires_at, Some(9_999));
        assert_eq!(
            app.refresh_token.as_deref(),
            Some("rt-ROTATED"),
            "next refresh must use the rotated token, not the revoked original"
        );
        assert!(!app.reauth_needed);
        assert!(!app.refresh_disabled);
    }

    #[test]
    fn failed_refresh_latches_reauth_and_stops_hot_looping() {
        let mut app = test_app();
        let follow = apply_msg(&mut app, Msg::TokenRefreshed(Err(anyhow!("invalid_grant"))));
        assert!(matches!(follow, FollowUp::None));
        assert!(app.reauth_needed, "surface re-auth in the status bar");
        assert!(
            app.refresh_disabled,
            "latch so maybe_spawn_token_refresh can't re-fire the dead token every interval"
        );

        // With the latch set, maybe_spawn_token_refresh must short-circuit even though the token is
        // expired and a refresh token is present — i.e. no further IdP calls until re-login.
        app.token_expires_at = Some(auth::now_unix() - 10); // expired
        let mut last = Instant::now() - REFRESH_INTERVAL * 2;
        let cfg = Config::resolve(&Default::default()).unwrap();
        let http = reqwest::Client::new();
        let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
        maybe_spawn_token_refresh(&mut app, &cfg, &http, &tx, &mut last);
        assert!(
            rx.try_recv().is_err(),
            "no refresh task should be spawned once refresh_disabled is latched"
        );
    }

    #[test]
    fn successful_mutation_requests_immediate_view_refresh() {
        let mut app = test_app();
        let repo = RepositoryRow {
            id: 1,
            github_repo_id: 1,
            owner: "o".into(),
            name: "r".into(),
            default_branch: "main".into(),
            status: "approved".into(),
            active: true,
            approved_at: None,
            approved_by: None,
            task_count: 0,
            last_task_at: None,
        };
        let follow = apply_msg(
            &mut app,
            Msg::RepoAction {
                verb: "approved",
                result: Ok(repo),
            },
        );
        assert!(matches!(follow, FollowUp::RefreshView));
    }
}
