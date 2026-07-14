-- Ticket #246: OTel distributed tracing. A W3C `traceparent` string
-- (`version-traceid-spanid-flags`), captured at webhook receipt and re-parented at each hop, is
-- persisted here because a live tracing span can't survive the gaps this pipeline has: an
-- arbitrary dispatch delay before the Job launches, the Job being a separate OS process, and an
-- arbitrary outbox retry/backoff delay (possibly across a control-plane restart) before egress.
--
-- `outbox.trace_context` is populated from `tasks.trace_context` via a subquery at enqueue time
-- (see `enqueue_outbox_post` in db/outbox.rs), not by every producer passing it explicitly — an
-- outbox row not tied to a task (`task_id IS NULL`) simply gets `trace_context IS NULL`, and its
-- egress span starts its own independently-sampled root rather than failing.
ALTER TABLE tasks ADD COLUMN IF NOT EXISTS trace_context TEXT;
ALTER TABLE outbox ADD COLUMN IF NOT EXISTS trace_context TEXT;
