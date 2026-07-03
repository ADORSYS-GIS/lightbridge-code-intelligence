//! The async TUI event loop. Wires crossterm input, a periodic refresh timer, and an mpsc channel of
//! async API results into the [`App`] state machine, and renders each frame via [`ui::draw`].
//!
//! Networking never runs on the render path: key actions spawn tasks that post their result back
//! over the channel, so the UI stays responsive.

mod app;
mod terminal;
mod ui;

pub use app::App;

use crate::api::{ApiClient, Me, RepositoryRow, TaskRow};
use crate::auth::{self, EXPIRY_SKEW_SECS};
use crate::config::Config;
use anyhow::Result;
use app::{PendingAction, View};
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use futures::StreamExt;
use std::time::{Duration, Instant};
use terminal::TerminalGuard;
use tokio::sync::mpsc;

/// Auto-refresh cadence.
const REFRESH_INTERVAL: Duration = Duration::from_secs(5);

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
) -> Result<()> {
    let mut guard = TerminalGuard::enter()?;
    let mut app = App::new(me, api.host(), token_expires_at, refresh_token);

    let (tx, mut rx) = mpsc::unbounded_channel::<Msg>();
    let mut input = EventStream::new();
    let mut refresh_timer = tokio::time::interval(REFRESH_INTERVAL);
    // The first tick fires immediately; we also kick an initial load below.
    let mut ui_tick = tokio::time::interval(Duration::from_millis(250));

    // Kick the initial data + a token-refresh watchdog state.
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
                        handle_key(&mut app, key, &api, &tx);
                    }
                    Some(Ok(Event::Resize(_, _))) => app.mark_dirty(),
                    Some(Err(_)) | None => break, // input stream closed
                    _ => {}
                }
            }

            // --- periodic auto-refresh of the active view ---
            _ = refresh_timer.tick() => {
                spawn_refresh_current_view(&app, &api, &tx);
                maybe_spawn_token_refresh(&mut app, &cfg, &http, &tx, &mut last_refresh_attempt);
            }

            // --- lightweight UI tick (toast expiry + token countdown redraw) ---
            _ = ui_tick.tick() => {
                app.tick_toast(Instant::now());
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
        }
    }

    // `guard` drops here → terminal restored.
    Ok(())
}

/// Translate a keypress into a state change and/or a spawned request.
fn handle_key(app: &mut App, key: KeyEvent, api: &ApiClient, tx: &mpsc::UnboundedSender<Msg>) {
    // A pending confirmation captures Enter/y/Esc/n first.
    if app.confirm.is_some() {
        match key.code {
            KeyCode::Enter | KeyCode::Char('y') => {
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
        KeyCode::Char('r') => {
            app.toast_info("refreshing…");
            spawn_refresh_current_view(app, api, tx);
        }
        KeyCode::Char('f') => {
            app.cycle_filter();
            spawn_refresh_current_view(app, api, tx);
        }
        KeyCode::Char('a') => request_approve(app),
        KeyCode::Char('d') => request_deny(app),
        KeyCode::Char('c') => request_cancel(app),
        _ => {}
    }
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
            format!("Approve {label}? This opens the gate and triggers indexing."),
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
            format!("Deny {label}? This disables it and PURGES its index data."),
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
        let id = task.id;
        app.ask_confirm(format!("Cancel run {id}?"), PendingAction::Cancel(id));
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
    }
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
            app.set_repos(repos);
            FollowUp::None
        }
        Msg::Repos(Err(e)) => {
            app.toast_error(format!("repos: {e}"));
            FollowUp::None
        }
        Msg::Tasks(Ok(tasks)) => {
            app.set_tasks(tasks);
            FollowUp::None
        }
        Msg::Tasks(Err(e)) => {
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
        App::new(me, "api.test".into(), 1_000, Some("rt-original".into()))
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
