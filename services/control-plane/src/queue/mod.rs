//! Queue & dispatch — the `dispatcher` role. Claim queued tasks under a lease and launch one Job
//! each, reap finished/cancelled Jobs, reconcile data purges, and own the task state machine.
//!
//! The `notifier` role (ADR-0079) also lives here: it mirrors the same claim/lease discipline to
//! deliver A2A push-notification webhooks from the `a2a_task_events` log.

pub(crate) mod dispatcher;
pub(crate) mod index_sweeper;
pub(crate) mod lifecycle;
pub(crate) mod notifier;
pub(crate) mod outbox_sweeper;
pub(crate) mod reaper;
pub(crate) mod reconciler;
pub(crate) mod tasks;
