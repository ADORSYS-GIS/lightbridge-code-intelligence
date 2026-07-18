-- ADR-0034 (epic #459 / #461): give the agent run transcript a dedicated `reasoning` column so
-- `content` and `reasoning` mean the SAME thing on both hosts (native agent-runner and OpenCode):
--   * `content`   — the model's VISIBLE message/answer text (what it concluded).
--   * `reasoning` — the model's CHAIN-OF-THOUGHT (`reasoning_content`) for the turn.
--
-- Why: the OpenCode host used to DROP the visible answer and write the model's reasoning INTO
-- `content` (F1 + F3), so `content` meant opposite things across the two hosts and the visible answer
-- was unrecoverable. This split fixes that at the storage layer; both hosts now populate `content`
-- with the answer and `reasoning` with the chain-of-thought.
--
-- Nullable, and only assistant turns carry it (tool-result rows stay NULL, as with the token/model
-- columns). Historical rows are left untouched: a pre-migration OpenCode run kept its reasoning in
-- `content` and its `reasoning` reads back NULL — those rows stay readable, just with the old
-- (host-dependent) semantics; no backfill is attempted (the original visible answer was never stored,
-- so it cannot be reconstructed).
ALTER TABLE agent_transcript
    ADD COLUMN IF NOT EXISTS reasoning TEXT;
