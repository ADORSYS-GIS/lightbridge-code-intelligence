-- ADR-0111: check-run / commit-status reporting. When a review runner starts work on a PR/MR's head
-- SHA, the reconciler posts an "in progress" signal (GitHub Check Run / GitLab commit status /
-- Bitbucket build status); when the run finishes, it resolves that same check to the real outcome.
-- Rides the existing outbox (ADR-0059), not a new mechanism.

-- 1. Two new outbox kinds. Two kinds (not one kind + a phase payload field) because their payload
--    shapes genuinely differ (start carries no conclusion; resolve does) and every other outbox intent
--    is already one-kind-per-distinct-action — matching that precedent keeps `reconciler::deliver`'s
--    `match row.kind.as_str()` a closed, self-documenting case set.
ALTER TABLE outbox DROP CONSTRAINT IF EXISTS github_outbox_kind_check;
ALTER TABLE outbox DROP CONSTRAINT IF EXISTS outbox_kind_check;
ALTER TABLE outbox
    ADD CONSTRAINT outbox_kind_check
    CHECK (kind IN ('review', 'reply', 'reaction', 'label', 'failure_notice', 'pr_open',
                     'check_run_start', 'check_run_resolve'));

-- 2. Persist the GitHub check-run id between the (asynchronous, independently-retried) start and
--    resolve outbox rows. NOT reusing outbox.platform_ref_id: that column is scoped to ONE outbox row
--    (set once by mark_outbox_posted), but start and resolve are two separate rows delivered at two
--    separate times by two separate producer call sites — the resolve delivery has no row-local way to
--    read the start row's platform_ref_id without an extra correlated query, and that query would still
--    race a start row that hasn't posted yet (queued/retrying) or dead-lettered (403, permission
--    missing). tasks already carries the per-task, single-source-of-truth fields this check run's
--    identity belongs next to (head_sha, base_sha) and is already loaded by every call site that needs
--    to read this back (finalize_review / handle_review_failure / the reaper, all via
--    db::get_task_context). NULL means "no check run id was ever recorded" — read by the GitHub
--    self-healing fallback (create-already-resolved in one call) at resolve time. GitLab/Bitbucket
--    never populate this (their status APIs upsert by sha, no id to remember).
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS check_run_external_id BIGINT;
