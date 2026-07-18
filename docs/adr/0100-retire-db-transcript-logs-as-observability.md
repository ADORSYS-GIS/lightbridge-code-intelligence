# ADR-0100: Retire the DB run transcript; Loki logs are the single observability surface

- **Status:** Accepted (owner-directed 2026-07-18)
- **Date:** 2026-07-18
- **Deciders:** @stephane-segning
- **Source of truth:** epic #459, ticket #461

## Context and Problem Statement

[ADR-0034](0034-agent-run-transcript-and-observability.md) persisted a per-run `agent_transcript`
table (tool calls, reasoning, token usage) and surfaced it over `GET /tasks/{id}/transcript`. That was
designed against the **native** agent loop ([ADR-0026](0026-native-review-agent.md)), where we own the
loop and the transcript rode alongside the durable-step / checkpoint machinery
([ADR-0087](0087-durable-review-checkpoint-runtime.md)) — a real replay/resume seam.

Review now runs on **OpenCode over ACP** ([ADR-0097](0097-review-runs-on-opencode.md)). There we do
**not** own the loop: one `session/prompt` runs OpenCode's entire internal agent cycle and returns. The
recorder ([ADR-0095](0095-opencode-plugins-recording-and-gates.md)) → `transcript_from_recorder`
reconstruction is a **post-hoc record**, not execution state — a dead pod resumes nothing from it. So
on the live path the DB transcript's only remaining jobs were a durable audit trail and backing the
`clients/lci` TUI Run Detail view. An audit of a real prod run (task `73995ab6-…`) also showed the
reconstruction was lossy and semantically wrong: assistant `text` (content) parts were dropped and
reasoning was written into the `content` column (the F1/F3 findings behind #461's original framing).

Meanwhile the OpenCode **logger plugin** already reads `LCI_LOG_LEVEL`, writes winston-shaped JSON to
stderr → Loki, and sees every part + tool call in-process. Logs are the natural, already-deployed
observability surface — leveled, live, and not tied to a table.

## Decision

**Make Loki logs the single run-observability surface, and tear out the DB transcript subsystem
entirely — a hard cutover, no dormant code.**

Removed in this change (#461):

- **Both transcript writers.** The OpenCode `transcript_from_recorder` reconstruction (deleted) and
  the native `append_transcript` DB-entry building (reduced to logging only — see below). The
  `RecorderEvent.part` field and the recorder plugin's `reasoning.part` branch go with it (nothing
  reads them once the transcript is gone).
- **The API + persistence.** `POST /internal/tasks/{id}/transcript`, `GET /tasks/{id}/transcript`,
  `ingest_transcript`, `replace_transcript`, `get_transcript`, the `TranscriptEntry` /
  `TranscriptInput` / `TranscriptRow` types, and the agent-clients `submit_transcript`.
- **The TUI reader.** The `clients/lci` Run Detail transcript view (fetch + live-tail + render). The
  page now shows task metadata + review only.
- **The table.** Migration `0032_drop_agent_transcript.sql` `DROP`s `agent_transcript`, reverting
  `0014`/`0017`. Migrations are forward-only: this is irreversible in prod; historical rows are
  discarded.

**Kept:** the recorder file and its gate-facing parsing (`cycle_turn_outcome`, coverage accounting) —
that is a separate consumer from the transcript. And the **log lines**: the native path's per-turn
`agent reasoning` / `agent content` proof-of-work lines survive (the entry-building was split out from
the logging so the logging lives on). The OpenCode logger plugin gains leveled per-turn reasoning /
content / tool lines under **#462**, hardened by **#463** — this ADR clears the way; those tickets
deliver the "100% overview through multiple logging levels".

## Consequences

- **Positive.** One observability surface, not two; no lossy post-hoc reconstruction masquerading as
  execution state; no table to migrate/scale; the recorder's role narrows to exactly what still needs
  it (the quality gates).
- **Negative / transitional.** Between #461 and #462 the OpenCode path's reasoning/content is not
  emitted anywhere (the recorder branch is gone; the logger emission lands in #462). This is an
  accepted gap of the phased teardown. The `DROP TABLE` is irreversible in prod — a human must verify
  before it lands (AI Usage Declaration on the PR).
- This supersedes [ADR-0034](0034-agent-run-transcript-and-observability.md) and retires the
  reasoning-part recording described in [ADR-0095](0095-opencode-plugins-recording-and-gates.md); the
  recorder's tool-fidelity role for the gates is unchanged.
