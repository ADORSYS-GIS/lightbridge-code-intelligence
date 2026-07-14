//! The replay-then-tail SSE stream (ADR-0077) backing `SubscribeToTask` and the streaming leg of
//! `SendStreamingMessage` — [`super::A2aHandler::stream_task`] — plus its terminal-state backstop.
//! Split out of the former monolithic `a2a/handler.rs` (ADR-0086 follow-up code-quality pass) — pure
//! move, no behavior change.

use a2a::{A2AError, StreamResponse};
use futures::stream::BoxStream;
use sqlx::PgPool;
use sqlx::postgres::PgListener;
use uuid::Uuid;

use crate::a2a::mapping::task_state_from_status;
use crate::db;

use super::{A2aHandler, CallerCtx, MAX_STREAM_LIFETIME, TAIL_POLL_FALLBACK, db_error};

impl A2aHandler {
    /// The replay-then-tail SSE stream backing `SubscribeToTask` and the streaming leg of
    /// `SendStreamingMessage` (ADR-0077). Same caller-scoped ownership check as `GetTask`
    /// (unknown/foreign id → `TaskNotFound`); then: emit the initial `Task` snapshot, **replay** the
    /// task's `a2a_task_events` in `seq` order, then **tail** — waking on the per-task `NOTIFY` channel
    /// with a bounded fallback poll, re-querying `seq > last_emitted` each wake, and closing on the
    /// `final = true` event (or the terminal-state backstop). The `seq`-cursor SELECT is the source of
    /// truth; `NOTIFY` is only a wake hint, so a missed notify costs latency, never a lost/misordered
    /// event.
    pub(super) async fn stream_task(
        &self,
        caller: &CallerCtx,
        a2a_task_id: &str,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let id = Uuid::parse_str(a2a_task_id).map_err(|_| A2AError::task_not_found(a2a_task_id))?;
        let mapping = self
            .store
            .load_owned(id, &caller.id)
            .await
            .map_err(db_error)?
            .ok_or_else(|| A2AError::task_not_found(a2a_task_id))?;

        let snapshot = self.build_task_snapshot(a2a_task_id, &mapping).await?;
        let already_terminal = snapshot.status.state.is_terminal();

        // Per-caller concurrent-stream cap (RFC-0006 streaming fairness): take a slot up front so one
        // caller cannot hold more than `max_streams_per_caller` live streams on this replica — which is
        // what keeps a single caller from consuming the whole global `MAX_CONCURRENT_STREAMS` pool.
        // Acquired AFTER the ownership check (a not-found subscription consumes no slot) and the RAII
        // guard is moved INTO the stream below, so it releases when the stream ends: disconnect, clean
        // close, or error — whichever way the generator is dropped. Over the cap → the same clean
        // "capacity" error the global pool-saturation path returns.
        let Some(stream_slot) = self
            .stream_counters
            .try_acquire(&caller.id, self.limits.max_streams_per_caller)
        else {
            tracing::info!(caller = %caller.id, a2a_task_id = %id, "a2a: per-caller concurrent-stream cap reached");
            return Err(A2AError::internal(
                "streaming capacity reached; retry shortly or poll GetTask",
            ));
        };

        let pool = self.pool.clone();
        // Streams LISTEN on the dedicated, bounded listener pool — never the request pool (see
        // MAX_CONCURRENT_STREAMS): a long tail must not hold a request connection.
        let listener_pool = self.listener_pool.clone();
        let underlying = mapping.underlying_task_id;
        let channel = crate::a2a::events::task_channel(&id);

        let stream = async_stream::stream! {
            // Own the per-caller stream slot for the whole generator lifetime: it decrements the
            // caller's live-stream count when this stream is dropped (disconnect / close / error).
            let _stream_slot = stream_slot;

            // 1) Initial Task snapshot (the current GetTask view).
            yield Ok(StreamResponse::Task(snapshot));

            let mut last_seq: i64 = 0;

            // 2) Replay everything already logged (seq > 0), strictly in seq order.
            match crate::a2a::events::fetch_events_after(&pool, id, last_seq).await {
                Ok(events) => {
                    for (seq, payload, is_final) in events {
                        match serde_json::from_value::<StreamResponse>(payload) {
                            Ok(event) => yield Ok(event),
                            Err(error) => {
                                tracing::error!(%error, a2a_task_id = %id, seq, "a2a: corrupt stream event payload");
                                yield Err(A2AError::internal("internal error"));
                                return;
                            }
                        }
                        last_seq = seq;
                        if is_final {
                            return; // terminal event → close
                        }
                    }
                }
                Err(error) => {
                    yield Err(db_error(error));
                    return;
                }
            }

            // A task already terminal at subscribe replays its sequence and closes without tailing.
            if already_terminal {
                return;
            }

            // 3) Tail: NOTIFY-wake with a bounded fallback poll; the seq-cursor SELECT is authoritative.
            // The listener draws from the dedicated stream pool; when it is saturated (too many
            // concurrent streams on this replica) the acquire times out fast — surface that as a clear,
            // retryable "capacity" error rather than a generic internal failure.
            let mut listener = match PgListener::connect_with(&listener_pool).await {
                Ok(listener) => listener,
                Err(error) => {
                    tracing::warn!(%error, a2a_task_id = %id, "a2a: stream listener pool saturated or unavailable");
                    yield Err(A2AError::internal(
                        "streaming capacity reached; retry shortly or poll GetTask",
                    ));
                    return;
                }
            };
            if let Err(error) = listener.listen(&channel).await {
                tracing::error!(%error, a2a_task_id = %id, "a2a: stream LISTEN failed");
                yield Err(A2AError::internal("internal error"));
                return;
            }

            let start = tokio::time::Instant::now();
            loop {
                // Drain any events past the cursor (also covers events that landed between replay and
                // LISTEN — a missed notify never loses them because the cursor SELECT is the truth).
                match crate::a2a::events::fetch_events_after(&pool, id, last_seq).await {
                    Ok(events) => {
                        for (seq, payload, is_final) in events {
                            match serde_json::from_value::<StreamResponse>(payload) {
                                Ok(event) => yield Ok(event),
                                Err(error) => {
                                    tracing::error!(%error, a2a_task_id = %id, seq, "a2a: corrupt stream event payload");
                                    yield Err(A2AError::internal("internal error"));
                                    return;
                                }
                            }
                            last_seq = seq;
                            if is_final {
                                return;
                            }
                        }
                    }
                    Err(error) => {
                        yield Err(db_error(error));
                        return;
                    }
                }

                // Backstop (ADR-0077 risk S7): if the live state is already terminal, do one FINAL
                // drain before closing. The terminal `set_task_status` transaction commits the status
                // flip and its terminal events (COMPLETED + any artifact) atomically, so a
                // live-terminal read means those events are already durable. Returning here WITHOUT
                // re-draining would drop them whenever the completion commits in the window between the
                // drain at the top of this loop and this check — the stream would close after WORKING,
                // never delivering the terminal event (an R6 violation: the stream must close *at* the
                // terminal state, delivering it). Re-fetch once; in the genuine crash case (the run
                // finished but no terminal event was ever appended) this finds nothing and we still
                // close rather than tail forever.
                if is_live_terminal(&pool, underlying).await {
                    match crate::a2a::events::fetch_events_after(&pool, id, last_seq).await {
                        Ok(events) => {
                            // Final drain: emit whatever the terminal commit left past the cursor, then
                            // close unconditionally (no need to advance `last_seq` — we never loop again).
                            for (seq, payload, is_final) in events {
                                match serde_json::from_value::<StreamResponse>(payload) {
                                    Ok(event) => yield Ok(event),
                                    Err(error) => {
                                        tracing::error!(%error, a2a_task_id = %id, seq, "a2a: corrupt stream event payload");
                                        yield Err(A2AError::internal("internal error"));
                                        return;
                                    }
                                }
                                if is_final {
                                    return;
                                }
                            }
                        }
                        Err(error) => {
                            yield Err(db_error(error));
                            return;
                        }
                    }
                    return;
                }

                // Per-connection max lifetime caps a truly stuck tail (ADR-0077 S2/S7).
                if start.elapsed() >= MAX_STREAM_LIFETIME {
                    tracing::warn!(a2a_task_id = %id, "a2a: stream hit max lifetime; closing tail");
                    return;
                }

                // Wake on NOTIFY or the fallback poll, whichever first. Errors/timeouts just re-loop and
                // re-query the cursor.
                let _ = tokio::time::timeout(TAIL_POLL_FALLBACK, listener.recv()).await;
            }
        };

        Ok(Box::pin(stream))
    }
}

/// Whether the live underlying-task state is terminal — the streaming tail's terminal-close backstop
/// (ADR-0077 S7). A reaped underlying row (or no underlying at all) reads as terminal (nothing more can
/// happen); a transient DB error reads as NOT terminal so the tail keeps polling rather than closing
/// early on a blip.
async fn is_live_terminal(pool: &PgPool, underlying: Option<Uuid>) -> bool {
    match underlying {
        Some(underlying) => match db::get_task(pool, underlying).await {
            Ok(Some(task)) => task_state_from_status(&task.status).is_terminal(),
            Ok(None) => true,
            Err(error) => {
                tracing::warn!(%error, "a2a: stream terminal backstop query failed (keep tailing)");
                false
            }
        },
        None => true,
    }
}
