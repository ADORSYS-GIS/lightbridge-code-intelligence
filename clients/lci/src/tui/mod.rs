//! The async TUI event loop. Wires crossterm input, a periodic refresh timer, and an mpsc channel of
//! async API results into the [`App`] state machine, and renders each frame via [`ui::draw`].
//!
//! Networking never runs on the render path: key actions spawn tasks that post their result back
//! over the channel, so the UI stays responsive.
//!
//! This module is the thin **composition root** of the TUI's Elm-ish architecture — it just wires the
//! pieces together. The pieces themselves live in siblings:
//! - [`app`] — the Model (`App`'s data + queries) and its Update transitions (`tui::app::update`).
//! - [`update`] — the terminal-facing Update half: decode a crossterm key/mouse event into a
//!   transition on `App`, `match`ed on key code / screen, not `if`/`else if` chains.
//! - [`message`] — the async `Msg`/`Cmd` plumbing: spawned network requests and folding their
//!   results back into `App` state.
//! - [`ui`] — the View: pure rendering, split by screen/panel.

pub(crate) mod app;
mod message;
mod terminal;
pub(crate) mod ui;
mod update;

pub use app::App;

use crate::api::{ApiClient, Me};
use crate::config::Config;
use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyEventKind};
use futures::StreamExt;
use message::FollowUp;
use std::time::{Duration, Instant};
use terminal::TerminalGuard;
use tokio::sync::mpsc;

/// Auto-refresh cadence.
const REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// Live-tail poll cadence for the open detail page (2–3s per the ticket). Faster than the list
/// refresh so a running agent's new turns show promptly.
const DETAIL_POLL_INTERVAL: Duration = Duration::from_millis(2500);

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

    let (tx, mut rx) = mpsc::unbounded_channel::<message::Msg>();
    let mut input = EventStream::new();
    let mut refresh_timer = tokio::time::interval(REFRESH_INTERVAL);
    // The first tick fires immediately; we also kick an initial load below.
    let mut ui_tick = tokio::time::interval(Duration::from_millis(250));
    // The live-tail poll for an open detail page (fires only while one is open + still live).
    let mut detail_poll = tokio::time::interval(DETAIL_POLL_INTERVAL);

    // Kick the initial data + a token-refresh watchdog state.
    app.set_loading(true);
    message::spawn_refresh_current_view(&app, &api, &tx);
    let mut last_refresh_attempt = Instant::now();

    // Initial draw.
    guard.terminal.draw(|f| ui::draw(f, &app))?;

    loop {
        tokio::select! {
            // --- keyboard / terminal input ---
            maybe_event = input.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        update::handle_key(&mut app, key, &api, &tx, &mut guard);
                    }
                    Some(Ok(Event::Mouse(m))) => update::handle_mouse(&mut app, m),
                    Some(Ok(Event::Resize(_, _))) => app.mark_dirty(),
                    Some(Err(_)) | None => break, // input stream closed
                    _ => {}
                }
            }

            // --- periodic auto-refresh of the active view ---
            _ = refresh_timer.tick() => {
                app.set_loading(true);
                message::spawn_refresh_current_view(&app, &api, &tx);
                message::maybe_spawn_token_refresh(&mut app, &cfg, &http, &tx, &mut last_refresh_attempt);
            }

            // --- live-tail poll for an open detail page (status + transcript) ---
            _ = detail_poll.tick() => {
                // `should_poll` includes the in-flight guard, so a slow backend can't stack polls.
                if let Some(d) = app.detail.as_mut()
                    && d.should_poll() {
                        let id = d.task_id;
                        d.tail_in_flight = true;
                        message::spawn_detail_tail(id, &api, &tx);
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
                match message::apply_msg(&mut app, msg) {
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
            message::spawn_refresh_current_view(&app, &api, &tx);
        }

        if app.should_quit {
            break;
        }
        if app.take_dirty() {
            guard.terminal.draw(|f| ui::draw(f, &app))?;
            // The detail renderer measured the transcript geometry into `DetailState`'s cells during
            // the draw; reconcile the scroll offset (autoscroll pin / clamp) now. If it moved, a
            // follow-up redraw shows the corrected position without waiting for the next event.
            if let Some(d) = app.detail.as_mut()
                && d.sync_after_render()
            {
                guard.terminal.draw(|f| ui::draw(f, &app))?;
            }
        }
    }

    // `guard` drops here → terminal restored (mouse capture disabled included).
    Ok(())
}
