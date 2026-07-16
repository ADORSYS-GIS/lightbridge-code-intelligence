//! The async "command" plumbing: the [`Msg`] enum a spawned request posts back over the channel, the
//! functions that spawn those requests ([`dispatch_action`], `spawn_*`, [`maybe_spawn_token_refresh`]),
//! and [`apply_msg`], which folds a resolved `Msg` into [`App`] state. This is the Elm-architecture
//! "`Cmd`" side: network I/O never runs on the render path, so a key action or timer tick spawns a
//! task here that reports its result back through `tx`.

use super::REFRESH_INTERVAL;
use super::app::{App, PendingAction, View};
use crate::api::{ApiClient, RepositoryRow, ReviewRow, TaskRow, TranscriptRow};
use crate::auth::{self, EXPIRY_SKEW_SECS};
use crate::config::Config;
use anyhow::Result;
use std::time::Instant;
use tokio::sync::mpsc;
use uuid::Uuid;

/// A result posted back from a spawned async request task.
pub(super) enum Msg {
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

/// A follow-up the event loop performs after folding a message into state — things that need `&api`
/// or must be `async` (swapping the live bearer, re-fetching a view). Keeps [`apply_msg`] sync.
pub(super) enum FollowUp {
    None,
    /// A refresh produced a new access token → swap the client's live bearer to it.
    SwapBearer(String),
    /// A mutation succeeded → re-fetch the current view now (don't wait for the periodic refresh).
    RefreshView,
}

/// Fold an async result into the state, returning any follow-up the loop must run.
pub(super) fn apply_msg(app: &mut App, msg: Msg) -> FollowUp {
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

/// Spawn the network call for a confirmed action.
pub(super) fn dispatch_action(
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
pub(super) fn spawn_refresh_current_view(
    app: &App,
    api: &ApiClient,
    tx: &mpsc::UnboundedSender<Msg>,
) {
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
pub(super) fn spawn_detail_fetch(
    id: Uuid,
    app: &mut App,
    api: &ApiClient,
    tx: &mpsc::UnboundedSender<Msg>,
) {
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
pub(super) fn spawn_detail_tail(id: Uuid, api: &ApiClient, tx: &mpsc::UnboundedSender<Msg>) {
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
pub(super) fn maybe_spawn_token_refresh(
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
            platform_repo_id: 1,
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
